//! `residue agents prune`: move old Codex session logs into residue's trash.
//!
//! The only deletion that is not a Git worktree. It moves
//! `$CODEX_HOME/sessions/**/rollout-*.jsonl` and `rollout-*.jsonl.zst` with
//! one rename(2) into the trash beside the stats ledger, after a plan the
//! user confirms. It never reads a rollout, never calls `codex delete`, and
//! `watch` and `service` have no path here.
//!
//! Decision, 2026-11-19, Codex CLI 0.170.0, a scratch `CODEX_HOME` with two
//! synthetic rollout files and no logged-in session: `codex delete <uuid>`
//! exited 1 (`No active or archived session found`) and left both files,
//! because that command reads Codex's sqlite index, not loose files.
//! Current Codex `find_thread_path_by_id_str_in_subdir` logs a missing db
//! path as `stale_db_path` and falls back to a directory search. The trash
//! can put the files back with rename(2). A real logged-in session was not
//! created for that check.

use std::ffi::{CString, OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, IsTerminal, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use crate::activity::Snapshot;
use crate::cli::{Options, Span};
use crate::commands::{self, VERSION};
use crate::fingerprint::{self, Hasher};
use crate::hold::{self, Taken};
use crate::json::Json;
use crate::output::{self, clean};
use crate::stats::{self, Event, Line};

const SCHEMA: &str = "residue.agents-prune/v1";
const DIR_NAME: &str = "agent-trash";

#[derive(Clone)]
struct Identity {
    dev: u64,
    ino: u64,
    size: u64,
    mtime: i64,
    nsec: i64,
}

impl Identity {
    fn of(meta: &fs::Metadata) -> Identity {
        Identity {
            dev: meta.dev(),
            ino: meta.ino(),
            size: meta.size(),
            mtime: meta.mtime(),
            nsec: meta.mtime_nsec(),
        }
    }

    fn changed(&self, now: &Identity) -> Option<&'static str> {
        if self.dev != now.dev || self.ino != now.ino {
            Some("its inode")
        } else if self.size != now.size {
            Some("its size")
        } else if self.mtime != now.mtime || self.nsec != now.nsec {
            Some("its mtime")
        } else {
            None
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Act {
    Trash,
    Skip,
    Refuse,
}

impl Act {
    fn code(self) -> &'static str {
        match self {
            Act::Trash => "trash",
            Act::Skip => "skip",
            Act::Refuse => "refuse",
        }
    }
}

struct FilePlan {
    path: PathBuf,
    thread: String,
    id: Option<Identity>,
    bytes: u64,
    mtime: i64,
    act: Act,
    /// Set when the file will not move. Empty when it will.
    reason: String,
}

struct PurgePlan {
    entry: String,
    dir: PathBuf,
    files: Vec<RecFile>,
    act: Act,
    reason: String,
}

#[derive(Clone)]
struct RecFile {
    path: PathBuf,
    thread: String,
    id: Identity,
    /// Name inside the entry's `files/` directory: `0`, `1`, ...
    slot: String,
}

struct Record {
    codex_home: PathBuf,
    trashed: i64,
    keep_seconds: i64,
    keep_text: String,
    files: Vec<RecFile>,
}

/// One trash entry `residue restore` can put back.
struct Listed {
    name: String,
    dir: PathBuf,
    id: String,
    record: Result<Record, String>,
}

pub fn command(options: &Options) -> u8 {
    let started = output::now();
    let as_of = options.as_of.map_or(started, |t| t.min(started));
    let older = match &options.older_than {
        Some(span) => span.clone(),
        None => return commands::fail("--older-than is required"),
    };
    let keep = options.keep_trash();
    let home = match home_dir() {
        Ok(path) => path,
        Err(message) => return commands::fail(&message),
    };
    let (codex, source) = codex_home(&home);
    let sessions = codex.join("sessions");
    let mut warnings = Vec::new();
    let mut errors = Vec::new();
    let snapshot = Snapshot::take();
    let gate = activity_gate(&snapshot);
    let flock_ok = match flock_supported(&sessions) {
        Ok(ok) => ok,
        Err(message) => {
            errors.push(message);
            false
        }
    };
    let mut files = match classify(
        &sessions,
        &older,
        as_of,
        gate.as_deref(),
        flock_ok,
        &mut errors,
    ) {
        Ok(files) => files,
        Err(message) => {
            errors.push(message);
            Vec::new()
        }
    };
    let mut purges = match plan_purges(&keep, as_of) {
        Ok(purges) => purges,
        Err(message) => {
            warnings.push(message);
            Vec::new()
        }
    };
    let plan = fingerprint(&codex, &older, &keep, &files, &purges);
    let differs = options.expect.as_deref().is_some_and(|want| want != plan);
    let act = !differs && will_act(&files, &purges);
    if differs {
        mark_unmoved(&mut files, &mut purges, PLAN_DIFFERS);
    } else if act && options.yes {
        apply(
            &mut files,
            &mut purges,
            &codex,
            &older,
            &keep,
            as_of,
            &mut warnings,
        );
    } else if act {
        match confirm(count_act(&files), count_purge(&purges)) {
            Ok(true) => {
                apply(
                    &mut files,
                    &mut purges,
                    &codex,
                    &older,
                    &keep,
                    as_of,
                    &mut warnings,
                );
            }
            Ok(false) => {
                mark_unmoved(
                    &mut files,
                    &mut purges,
                    "you did not type yes, so nothing was moved",
                );
            }
            Err(message) => mark_unmoved(&mut files, &mut purges, &message),
        }
    }
    for warning in &warnings {
        eprintln!("residue: warning: {}", clean(warning));
    }
    for error in &errors {
        eprintln!("residue: {}", clean(error));
    }
    let body = document(&Doc {
        started,
        as_of,
        older: &older,
        codex: &codex,
        source,
        plan: &plan,
        expect: options.expect.as_deref(),
        files: &files,
        purges: &purges,
        errors: &errors,
        warnings: &warnings,
    });
    if !write_stdout(&body) {
        return 1;
    }
    if differs {
        fingerprint::CHANGED
    } else if !errors.is_empty()
        && !will_act(&files, &purges)
        && purges.iter().all(|p| p.act != Act::Trash)
    {
        // A store that could not be listed, and nothing was purged.
        if purges
            .iter()
            .any(|p| p.act == Act::Skip || p.act == Act::Refuse)
        {
            3
        } else {
            1
        }
    } else if all_done(&files, &purges) {
        0
    } else {
        3
    }
}

const PLAN_DIFFERS: &str = "the plan is not the one --expect names: something changed since that plan was made, so nothing was moved, purged or restored";

fn home_dir() -> Result<PathBuf, String> {
    match std::env::var_os("HOME") {
        Some(value) if !value.is_empty() && value != "/" => Ok(PathBuf::from(value)),
        _ => Err("HOME is unset or /, so residue cannot find Codex's directory".into()),
    }
}

fn codex_home(home: &Path) -> (PathBuf, &'static str) {
    match std::env::var_os("CODEX_HOME").filter(|v| !v.is_empty()) {
        Some(value) => (PathBuf::from(value), "CODEX_HOME"),
        None => (home.join(".codex"), "default"),
    }
}

/// `Some(reason)` when the probe cannot show that a file is unused, which
/// keeps every file in the run.
fn activity_gate(snapshot: &Snapshot) -> Option<String> {
    match &snapshot.state {
        Err(unavailable) => Some(format!(
            "activity is unknown ({}), so nothing was moved: {}",
            unavailable.reason, unavailable.detail
        )),
        Ok(seen) if seen.limits.is_some() => Some(format!(
            "activity is unknown (partial_probe), so nothing was moved: {} reads {}, so it cannot show that no process has a session open",
            snapshot.backend,
            seen.limits.unwrap_or("less than its open files")
        )),
        Ok(seen) if seen.hidden.is_some() => Some(format!(
            "activity is unknown (hidden_processes), so nothing was moved: {}",
            seen.hidden
                .as_ref()
                .map(|h| h.detail.as_str())
                .unwrap_or("residue cannot see every process")
        )),
        Ok(_) => None,
    }
}

/// `Ok(false)` when `dir`'s filesystem gives no flock(2). A missing
/// directory is fine: there is nothing to move there.
fn flock_supported(dir: &Path) -> Result<bool, String> {
    if !dir.exists() {
        return Ok(true);
    }
    let meta = fs::symlink_metadata(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    if meta.file_type().is_symlink() {
        return Err(format!(
            "{} is a symbolic link, so residue does not list it and moved nothing",
            dir.display()
        ));
    }
    if !meta.is_dir() {
        return Err(format!("{} is not a directory", dir.display()));
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(dir)
        .map_err(|e| format!("cannot open {}: {e}", dir.display()))?;
    match flock_exclusive(&file) {
        Ok(_) => Ok(true),
        Err(e) if flock_unsupported(&e) => Ok(false),
        Err(e) => Err(format!(
            "cannot tell whether {} has flock(2) ({e})",
            dir.display()
        )),
    }
}

fn flock_exclusive(file: &File) -> io::Result<bool> {
    loop {
        // SAFETY: flock on a descriptor this process opened. LOCK_NB so a
        // busy lock is an answer, not a wait. The lock is dropped when
        // `file` is dropped at the end of this probe.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            // SAFETY: the same descriptor, unlocked before it is closed.
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
            return Ok(true);
        }
        let err = io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::EINTR) => {}
            Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN => return Ok(false),
            _ => return Err(err),
        }
    }
}

fn flock_unsupported(err: &io::Error) -> bool {
    err.raw_os_error().is_some_and(|code| {
        code == libc::ENOLCK
            || code == libc::ENOTSUP
            || code == libc::EOPNOTSUPP
            || code == libc::ENOSYS
    })
}

fn classify(
    sessions: &Path,
    older: &Span,
    as_of: i64,
    gate: Option<&str>,
    flock_ok: bool,
    errors: &mut Vec<String>,
) -> Result<Vec<FilePlan>, String> {
    let meta = match fs::symlink_metadata(sessions) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("{}: {e}", sessions.display())),
        Ok(meta) => meta,
    };
    if meta.file_type().is_symlink() {
        return Err(format!(
            "{} is a symbolic link, so residue does not list it and moved nothing",
            sessions.display()
        ));
    }
    if !meta.is_dir() {
        return Err(format!("{} is not a directory", sessions.display()));
    }
    let mut found = Vec::new();
    walk(sessions, &mut found, errors)?;
    if !errors.is_empty() {
        // An incomplete list must not choose a thread. Report every rollout
        // we did see as skipped, and move none of them.
        for file in &mut found {
            file.act = Act::Skip;
            file.reason =
                "the sessions directory could not be listed completely, so nothing was moved"
                    .into();
        }
        return Ok(found);
    }
    if let Some(why) = gate {
        for file in &mut found {
            if file.act == Act::Trash {
                file.act = Act::Skip;
                file.reason = why.to_string();
            }
        }
        return Ok(found);
    }
    if !flock_ok {
        for file in &mut found {
            if file.act == Act::Trash {
                file.act = Act::Refuse;
                file.reason = format!(
                    "the filesystem of {} gives no flock(2) lock, so nothing was moved",
                    sessions.display()
                );
            }
        }
        return Ok(found);
    }
    apply_age_and_threads(&mut found, older, as_of);
    apply_activity(&mut found);
    Ok(found)
}

fn walk(dir: &Path, found: &mut Vec<FilePlan>, errors: &mut Vec<String>) -> Result<(), String> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            errors.push(format!("{}: {e}", dir.display()));
            return Ok(());
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                errors.push(format!("{}: {e}", dir.display()));
                continue;
            }
        };
        let path = entry.path();
        let name = entry.file_name();
        let meta = match fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(e) => {
                errors.push(format!("{}: {e}", path.display()));
                continue;
            }
        };
        if meta.file_type().is_symlink() {
            if is_rollout(&name) {
                found.push(FilePlan {
                    path,
                    thread: thread_id(&name),
                    id: None,
                    bytes: 0,
                    mtime: 0,
                    act: Act::Refuse,
                    reason: "it is a symbolic link, so it was not followed and nothing was moved"
                        .into(),
                });
            }
            continue;
        }
        if meta.is_dir() {
            walk(&path, found, errors)?;
            continue;
        }
        if meta.is_file() && is_rollout(&name) {
            let id = Identity::of(&meta);
            found.push(FilePlan {
                thread: thread_id(&name),
                path,
                bytes: id.size,
                mtime: id.mtime,
                id: Some(id),
                act: Act::Trash,
                reason: String::new(),
            });
        }
    }
    Ok(())
}

fn is_rollout(name: &OsStr) -> bool {
    let bytes = name.as_bytes();
    bytes.starts_with(b"rollout-") && (bytes.ends_with(b".jsonl") || bytes.ends_with(b".jsonl.zst"))
}

/// The thread id is the UUID in the name. `rollout-TIME-UUID.jsonl` and
/// `rollout-TIME-UUID_2.jsonl` are one thread. A name with no UUID is its
/// own thread, so two unrelated files are not moved together.
fn thread_id(name: &OsStr) -> String {
    let text = name.to_string_lossy();
    let mut stem = text.strip_prefix("rollout-").unwrap_or(text.as_ref());
    if let Some(stripped) = stem.strip_suffix(".jsonl.zst") {
        stem = stripped;
    } else if let Some(stripped) = stem.strip_suffix(".jsonl") {
        stem = stripped;
    }
    if let Some(index) = stem.rfind('_') {
        let tail = &stem[index + 1..];
        if !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()) {
            stem = &stem[..index];
        }
    }
    uuid_suffix(stem).unwrap_or(stem).to_string()
}

fn uuid_suffix(stem: &str) -> Option<&str> {
    if stem.len() < 36 {
        return None;
    }
    let id = &stem[stem.len() - 36..];
    let bytes = id.as_bytes();
    let hyphens = [8usize, 13, 18, 23];
    if hyphens.iter().any(|i| bytes[*i] != b'-') {
        return None;
    }
    if bytes
        .iter()
        .enumerate()
        .any(|(i, b)| !hyphens.contains(&i) && !b.is_ascii_hexdigit())
    {
        return None;
    }
    if stem.len() != 36 && !stem[..stem.len() - 36].ends_with('-') {
        return None;
    }
    Some(id)
}

fn apply_age_and_threads(files: &mut [FilePlan], older: &Span, as_of: i64) {
    let mut threads: Vec<String> = files.iter().map(|f| f.thread.clone()).collect();
    threads.sort();
    threads.dedup();
    for thread in threads {
        let members: Vec<usize> = files
            .iter()
            .enumerate()
            .filter(|(_, f)| f.thread == thread)
            .map(|(i, _)| i)
            .collect();
        let refused = members.iter().any(|i| files[*i].act == Act::Refuse);
        let unreadable = members.iter().any(|i| files[*i].id.is_none());
        let cutoff = as_of.saturating_sub(older.seconds);
        let fresh = members
            .iter()
            .any(|i| files[*i].id.as_ref().is_some_and(|id| id.mtime > cutoff));
        if refused {
            for i in members {
                if files[i].act != Act::Refuse {
                    files[i].act = Act::Skip;
                    files[i].reason =
                        "a symbolic link in this thread was refused, so the thread stays".into();
                }
            }
        } else if unreadable {
            for i in &members {
                files[*i].act = Act::Skip;
                files[*i].reason =
                    "a file in this thread has no readable mtime, so the thread stays".into();
            }
        } else if fresh {
            for i in &members {
                files[*i].act = Act::Skip;
                files[*i].reason = format!(
                    "a file in this thread changed within {}, so the thread stays",
                    older.text
                );
            }
        }
    }
}

fn apply_activity(files: &mut [FilePlan]) {
    let snapshot = Snapshot::take();
    if let Some(why) = activity_gate(&snapshot) {
        for file in files.iter_mut().filter(|f| f.act == Act::Trash) {
            file.act = Act::Skip;
            file.reason = why.clone();
        }
        return;
    }
    let mut blocked = Vec::new();
    for file in files.iter().filter(|f| f.act == Act::Trash) {
        match snapshot.evidence(&file.path, None) {
            crate::activity::Evidence::InUse(holders) => {
                let who = holders
                    .first()
                    .map(|(pid, cmd)| format!("{cmd}[{pid}]"))
                    .unwrap_or_else(|| "a process".into());
                blocked.push((file.thread.clone(), format!("in use by {who}")));
            }
            crate::activity::Evidence::Unknown(unavailable) => {
                blocked.push((
                    file.thread.clone(),
                    format!(
                        "activity is unknown ({}), so nothing in the run was moved: {}",
                        unavailable.reason, unavailable.detail
                    ),
                ));
            }
            crate::activity::Evidence::NotObserved => {}
        }
    }
    let run_wide = blocked
        .iter()
        .any(|(_, why)| why.contains("nothing in the run"));
    if run_wide {
        let why = blocked
            .iter()
            .find(|(_, why)| why.contains("nothing in the run"))
            .map(|(_, why)| why.clone())
            .unwrap_or_else(|| "activity is unknown, so nothing was moved".into());
        for file in files.iter_mut().filter(|f| f.act == Act::Trash) {
            file.act = Act::Skip;
            file.reason = why.clone();
        }
        return;
    }
    for (thread, why) in blocked {
        for file in files
            .iter_mut()
            .filter(|f| f.act == Act::Trash && f.thread == thread)
        {
            file.act = Act::Skip;
            file.reason = why.clone();
        }
    }
}

fn plan_purges(keep: &Span, as_of: i64) -> Result<Vec<PurgePlan>, String> {
    let mut purges = Vec::new();
    for listed in list_entries()? {
        let Ok(record) = &listed.record else {
            purges.push(PurgePlan {
                entry: listed.name,
                dir: listed.dir,
                files: Vec::new(),
                act: Act::Skip,
                reason: "its record could not be read, so it stays".into(),
            });
            continue;
        };
        let due = record.trashed.saturating_add(record.keep_seconds) <= as_of;
        if !due {
            continue;
        }
        let _ = keep;
        purges.push(PurgePlan {
            entry: listed.name,
            dir: listed.dir,
            files: record.files.clone(),
            act: Act::Trash,
            reason: String::new(),
        });
    }
    Ok(purges)
}

fn will_act(files: &[FilePlan], purges: &[PurgePlan]) -> bool {
    files.iter().any(|f| f.act == Act::Trash) || purges.iter().any(|p| p.act == Act::Trash)
}

fn count_act(files: &[FilePlan]) -> usize {
    files.iter().filter(|f| f.act == Act::Trash).count()
}

fn count_purge(purges: &[PurgePlan]) -> usize {
    purges.iter().filter(|p| p.act == Act::Trash).count()
}

fn mark_unmoved(files: &mut [FilePlan], purges: &mut [PurgePlan], reason: &str) {
    for file in files.iter_mut().filter(|f| f.act == Act::Trash) {
        file.act = Act::Skip;
        file.reason = reason.to_string();
    }
    for purge in purges.iter_mut().filter(|p| p.act == Act::Trash) {
        purge.act = Act::Skip;
        purge.reason = reason.to_string();
    }
}

fn confirm(files: usize, purges: usize) -> Result<bool, String> {
    let stdin = io::stdin();
    if !stdin.is_terminal() {
        return Err(
            "stdin is not a terminal, so residue cannot ask for confirmation; pass --yes from the session that owns these sessions, once you have read the plan"
                .into(),
        );
    }
    let mut parts = Vec::new();
    if files > 0 {
        parts.push(format!(
            "Move {files} Codex session {} to the trash",
            if files == 1 { "file" } else { "files" }
        ));
    }
    if purges > 0 {
        parts.push(format!(
            "purge {purges} expired trash {}",
            if purges == 1 { "entry" } else { "entries" }
        ));
    }
    let prompt = format!("{}? Type yes to continue: ", parts.join(" and "));
    let mut err = io::stderr();
    write!(err, "{prompt}")
        .and_then(|_| err.flush())
        .map_err(|e| format!("cannot write the prompt: {e}"))?;
    let mut answer = String::new();
    stdin
        .lock()
        .read_line(&mut answer)
        .map_err(|e| format!("cannot read the answer: {e}"))?;
    Ok(answer.trim().eq_ignore_ascii_case("yes"))
}

fn apply(
    files: &mut [FilePlan],
    purges: &mut [PurgePlan],
    codex: &Path,
    older: &Span,
    keep: &Span,
    as_of: i64,
    warnings: &mut Vec<String>,
) {
    recheck(files);
    let moving: Vec<usize> = files
        .iter()
        .enumerate()
        .filter(|(_, f)| f.act == Act::Trash)
        .map(|(i, _)| i)
        .collect();
    if !moving.is_empty() {
        match move_files(files, &moving, codex, older, keep, as_of) {
            Ok(()) => {}
            Err(message) => {
                warnings.push(message.clone());
                for i in moving {
                    if files[i].act == Act::Trash {
                        files[i].act = Act::Skip;
                        files[i].reason = message.clone();
                    }
                }
            }
        }
    }
    for purge in purges.iter_mut().filter(|p| p.act == Act::Trash) {
        match purge_one(purge) {
            Ok(()) => {}
            Err(message) => {
                warnings.push(format!("{}: {message}", purge.entry));
                purge.act = Act::Skip;
                purge.reason = message;
            }
        }
    }
}

/// Right before a move: a file whose device, inode, size or mtime changed
/// keeps its whole thread.
fn recheck(files: &mut [FilePlan]) {
    let mut changed_threads = Vec::new();
    for file in files.iter().filter(|f| f.act == Act::Trash) {
        let Some(planned) = &file.id else {
            changed_threads.push((file.thread.clone(), "its mtime".to_string()));
            continue;
        };
        match fs::symlink_metadata(&file.path) {
            Ok(meta) if meta.file_type().is_symlink() || !meta.is_file() => {
                changed_threads
                    .push((file.thread.clone(), "it is no longer a regular file".into()));
            }
            Ok(meta) => {
                if let Some(what) = planned.changed(&Identity::of(&meta)) {
                    changed_threads.push((
                        file.thread.clone(),
                        format!("changed since the plan: {what}"),
                    ));
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                changed_threads.push((file.thread.clone(), "it is gone".into()));
            }
            Err(e) => changed_threads.push((file.thread.clone(), format!("cannot read it ({e})"))),
        }
    }
    for (thread, why) in changed_threads {
        for file in files
            .iter_mut()
            .filter(|f| f.thread == thread && f.act == Act::Trash)
        {
            file.act = Act::Skip;
            file.reason = why.clone();
        }
    }
}

fn move_files(
    files: &mut [FilePlan],
    moving: &[usize],
    codex: &Path,
    older: &Span,
    keep: &Span,
    as_of: i64,
) -> Result<(), String> {
    let root = trash_root()?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&root)
        .map_err(|e| format!("{}: {e}", root.display()))?;
    let name = format!("agents-{as_of}-{}", moving.len());
    let dir = unique_dir(&root, &name)?;
    let files_dir = dir.join("files");
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&files_dir)
        .map_err(|e| format!("{}: {e}", files_dir.display()))?;
    let mut record_files = Vec::new();
    for (slot, index) in moving.iter().copied().enumerate() {
        let file = &files[index];
        let Some(id) = &file.id else {
            return Err("a file has no identity".into());
        };
        record_files.push(RecFile {
            path: file.path.clone(),
            thread: file.thread.clone(),
            id: Identity {
                dev: id.dev,
                ino: id.ino,
                size: id.size,
                mtime: id.mtime,
                nsec: id.nsec,
            },
            slot: slot.to_string(),
        });
    }
    let record = Record {
        codex_home: codex.to_path_buf(),
        trashed: as_of,
        keep_seconds: keep.seconds,
        keep_text: keep.text.clone(),
        files: record_files,
    };
    let info = dir.join("info");
    write_record(&dir, &record, older)?;
    let _held = match hold::take(&info) {
        Ok(Taken::Held(held)) => held,
        Ok(Taken::Busy) => {
            return Err("another residue run holds this trash entry, so nothing was moved".into());
        }
        Ok(Taken::Gone) => return Err(format!("{} disappeared before the move", info.display())),
        Ok(Taken::Unsupported(why)) => return Err(why),
        Err(message) => return Err(message),
    };
    let mut bytes = 0u64;
    let mut moved = false;
    for (slot, index) in moving.iter().copied().enumerate() {
        let dest = files_dir.join(slot.to_string());
        let planned = files[index].id.as_ref().ok_or("a file has no identity")?;
        match rename_matching(&files[index].path, &dest, planned) {
            Ok(()) => {
                bytes = bytes.saturating_add(files[index].bytes);
                files[index].act = Act::Trash;
                files[index].reason.clear();
                moved = true;
            }
            Err(message) => {
                files[index].act = Act::Refuse;
                files[index].reason = message;
                let thread = files[index].thread.clone();
                let why = files[index].reason.clone();
                for other in files
                    .iter_mut()
                    .filter(|f| f.thread == thread && f.act == Act::Trash)
                {
                    other.act = Act::Skip;
                    other.reason = format!("another file in this thread was not moved: {why}");
                }
            }
        }
    }
    if moved {
        let entry = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or(name);
        if let Err(message) = write_ledger(Event::Trashed, None, bytes, codex, &entry) {
            eprintln!(
                "residue: warning: the stats ledger is not up to date ({message}); this does not change what was moved"
            );
        }
    }
    Ok(())
}

fn write_record(dir: &Path, record: &Record, older: &Span) -> Result<(), String> {
    let info_new = dir.join("info.new");
    let mut bytes =
        b"# residue agent trash. kind agent_files. residue restore ENTRY puts the files back.\n"
            .to_vec();
    bytes.extend_from_slice(b"codex_home ");
    bytes.extend_from_slice(record.codex_home.as_os_str().as_bytes());
    bytes.push(b'\n');
    bytes.extend_from_slice(
        format!(
            "trashed {} {}\nkeep {} {}\nolder_than {} {}\n",
            record.trashed,
            output::rfc3339(record.trashed),
            record.keep_seconds,
            record.keep_text,
            older.seconds,
            older.text
        )
        .as_bytes(),
    );
    for file in &record.files {
        let mut line = format!(
            "file {} {} {} {} {} {} ",
            file.id.dev,
            file.id.ino,
            file.id.size,
            file.id.mtime,
            file.id.nsec,
            file.thread.len()
        )
        .into_bytes();
        line.extend_from_slice(file.thread.as_bytes());
        line.push(b' ');
        line.extend_from_slice(file.slot.as_bytes());
        line.push(b' ');
        line.extend_from_slice(file.path.as_os_str().as_bytes());
        line.push(b'\n');
        bytes.extend_from_slice(&line);
    }
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&info_new)
            .map_err(|e| format!("{}: {e}", info_new.display()))?;
        file.write_all(&bytes)
            .and_then(|_| file.sync_all())
            .map_err(|e| format!("{}: {e}", info_new.display()))?;
    }
    let info = dir.join("info");
    fs::rename(&info_new, &info).map_err(|e| format!("{}: {e}", info.display()))?;
    if let Ok(parent) = File::open(dir) {
        let _ = parent.sync_all();
    }
    Ok(())
}

fn rename_matching(from: &Path, to: &Path, planned: &Identity) -> Result<(), String> {
    let parent = from
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| format!("{} has no parent directory", from.display()))?;
    let name = from
        .file_name()
        .ok_or_else(|| format!("{} has no file name", from.display()))?;
    let dest_parent = to
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| format!("{} has no parent directory", to.display()))?;
    let dest_name = to
        .file_name()
        .ok_or_else(|| format!("{} has no file name", to.display()))?;
    let src_dir = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(parent)
        .map_err(|e| format!("cannot open {}: {e}", parent.display()))?;
    let now = metadata_at(&src_dir, name).map_err(|e| {
        if e.raw_os_error() == Some(libc::ELOOP) {
            format!("{} is a symbolic link, so it was not moved", from.display())
        } else {
            format!("{}: {e}", from.display())
        }
    })?;
    if now.file_type().is_symlink() || !now.is_file() {
        return Err(format!("{} is not a regular file", from.display()));
    }
    let got = Identity::of(&now);
    if let Some(what) = planned.changed(&got) {
        return Err(format!("changed since the plan: {what}"));
    }
    let dst_dir = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(dest_parent)
        .map_err(|e| format!("cannot open {}: {e}", dest_parent.display()))?;
    renameat(&src_dir, name, &dst_dir, dest_name).map_err(|e| {
        if e.raw_os_error() == Some(libc::EXDEV) {
            format!(
                "{} is on another filesystem than the trash, so it was not copied and was not moved",
                from.display()
            )
        } else {
            format!("cannot move {}: {e}", from.display())
        }
    })
}

fn metadata_at(dir: &File, name: &OsStr) -> io::Result<fs::Metadata> {
    let file = openat(dir, name)?;
    file.metadata()
}

fn openat(dir: &File, name: &OsStr) -> io::Result<File> {
    let c_name = CString::new(name.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a file name contains a NUL"))?;
    // SAFETY: openat on a directory descriptor this process opened.
    // O_NOFOLLOW refuses a symlink. O_NONBLOCK so a fifo cannot wait.
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            c_name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a new descriptor from openat, not shared.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn renameat(from_dir: &File, from: &OsStr, to_dir: &File, to: &OsStr) -> io::Result<()> {
    let from_c = CString::new(from.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a file name contains a NUL"))?;
    let to_c = CString::new(to.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a file name contains a NUL"))?;
    // SAFETY: renameat between two directory descriptors this process
    // opened. Neither name is a path, so it cannot escape the directory.
    let rc = unsafe {
        libc::renameat(
            from_dir.as_raw_fd(),
            from_c.as_ptr(),
            to_dir.as_raw_fd(),
            to_c.as_ptr(),
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn purge_one(purge: &PurgePlan) -> Result<(), String> {
    let info = purge.dir.join("info");
    let _held = match hold::take(&info) {
        Ok(Taken::Held(held)) => held,
        Ok(Taken::Busy) => {
            return Err("another residue run holds it, so it stays until the next run".into());
        }
        Ok(Taken::Gone) => return Err("its record is gone".into()),
        Ok(Taken::Unsupported(why)) => return Err(why),
        Err(message) => return Err(message),
    };
    let files_dir = purge.dir.join("files");
    let mut kept = false;
    for file in &purge.files {
        let path = files_dir.join(&file.slot);
        match fs::symlink_metadata(&path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("{}: {e}", path.display())),
            Ok(meta) if meta.file_type().is_symlink() || !meta.is_file() => {
                kept = true;
                eprintln!(
                    "residue: warning: {} is no longer the file the record names, so it stays",
                    path.display()
                );
            }
            Ok(meta) => {
                if file.id.changed(&Identity::of(&meta)).is_some() {
                    kept = true;
                    eprintln!(
                        "residue: warning: {} no longer matches the record, so it stays",
                        path.display()
                    );
                    continue;
                }
                unlink_file(&files_dir, &file.slot)?;
            }
        }
    }
    if kept {
        return Err("a file in it did not match the record, so the entry stays".into());
    }
    let _ = fs::remove_dir(&files_dir);
    let _ = fs::remove_file(&info);
    fs::remove_dir(&purge.dir).map_err(|e| {
        format!(
            "its files were purged but the entry directory stays ({}: {e})",
            purge.dir.display()
        )
    })?;
    let bytes = purge
        .files
        .iter()
        .fold(0u64, |n, f| n.saturating_add(f.id.size));
    let codex = purge
        .dir
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    // The Codex home is not on the purge plan. The ledger still needs a
    // repo string; the entry name keeps the event unique.
    if let Err(message) = write_ledger(Event::Freed, None, bytes, &codex, &purge.entry) {
        eprintln!(
            "residue: warning: the stats ledger is not up to date ({message}); this does not change the purge"
        );
    }
    Ok(())
}

fn unlink_file(dir: &Path, name: &str) -> Result<(), String> {
    let parent = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(dir)
        .map_err(|e| format!("cannot open {}: {e}", dir.display()))?;
    let c_name =
        CString::new(name.as_bytes()).map_err(|_| format!("{name}: a file name contains a NUL"))?;
    // SAFETY: unlinkat of a direct child of a directory this process
    // opened. The name is not a path.
    let rc = unsafe { libc::unlinkat(parent.as_raw_fd(), c_name.as_ptr(), 0) };
    if rc == 0 {
        Ok(())
    } else {
        Err(format!(
            "cannot purge {}/{}: {}",
            dir.display(),
            name,
            io::Error::last_os_error()
        ))
    }
}

fn write_ledger(
    event: Event,
    from: Option<&str>,
    bytes: u64,
    codex: &Path,
    entry: &str,
) -> Result<(), String> {
    let line = Line {
        at: output::now(),
        event,
        by: Some("agents prune".into()),
        reason: Some("agents_prune".into()),
        from: from.map(str::to_string),
        bytes: Some(bytes),
        repo: codex.to_string_lossy().into_owned(),
        worktree: None,
        entry: entry.to_string(),
        seeded: false,
    };
    stats::record(None, vec![line])
}

fn unique_dir(root: &Path, name: &str) -> Result<PathBuf, String> {
    let mut dir = root.join(name);
    let mut n = 2u32;
    while dir.exists() {
        dir = root.join(format!("{name}-{n}"));
        n = n.saturating_add(1);
        if n == u32::MAX {
            return Err(format!("{}: too many entries", root.display()));
        }
    }
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(dir)
}

fn trash_root() -> Result<PathBuf, String> {
    Ok(stats::dir()?.join(DIR_NAME))
}

fn fingerprint(
    codex: &Path,
    older: &Span,
    keep: &Span,
    files: &[FilePlan],
    purges: &[PurgePlan],
) -> String {
    let mut hash = Hasher::start(b"residue.agents-prune/v1");
    hash.part(codex.as_os_str().as_bytes());
    hash.part(&older.seconds.to_le_bytes());
    hash.part(&keep.seconds.to_le_bytes());
    let mut order: Vec<usize> = (0..files.len()).collect();
    order.sort_by(|&a, &b| {
        files[a]
            .path
            .as_os_str()
            .as_bytes()
            .cmp(files[b].path.as_os_str().as_bytes())
    });
    for i in order {
        let file = &files[i];
        hash.part(file.path.as_os_str().as_bytes());
        hash.part(file.thread.as_bytes());
        hash.part(file.act.code().as_bytes());
        if let Some(id) = &file.id {
            hash.part(&id.dev.to_le_bytes());
            hash.part(&id.ino.to_le_bytes());
            hash.part(&id.size.to_le_bytes());
            hash.part(&id.mtime.to_le_bytes());
            hash.part(&id.nsec.to_le_bytes());
        }
    }
    let mut purges_order: Vec<usize> = (0..purges.len()).collect();
    purges_order.sort_by(|&a, &b| {
        purges[a]
            .dir
            .as_os_str()
            .as_bytes()
            .cmp(purges[b].dir.as_os_str().as_bytes())
    });
    for i in purges_order {
        let purge = &purges[i];
        hash.part(purge.dir.as_os_str().as_bytes());
        hash.part(purge.act.code().as_bytes());
        for file in &purge.files {
            hash.part(file.path.as_os_str().as_bytes());
            hash.part(&file.id.dev.to_le_bytes());
            hash.part(&file.id.ino.to_le_bytes());
            hash.part(&file.id.size.to_le_bytes());
            hash.part(&file.id.mtime.to_le_bytes());
            hash.part(&file.id.nsec.to_le_bytes());
        }
    }
    hash.hex()
}

fn all_done(files: &[FilePlan], purges: &[PurgePlan]) -> bool {
    let pending_move = files
        .iter()
        .any(|f| f.act == Act::Trash && !f.reason.is_empty());
    let refused = files.iter().any(|f| f.act == Act::Refuse);
    let skipped_after = files
        .iter()
        .any(|f| f.act == Act::Skip && is_failure_reason(&f.reason));
    let purge_bad = purges.iter().any(|p| !p.reason.is_empty());
    !pending_move && !refused && !skipped_after && !purge_bad
}

fn is_failure_reason(reason: &str) -> bool {
    reason.starts_with("the plan is not")
        || reason.starts_with("stdin is not a terminal")
        || reason.starts_with("you did not type yes")
        || reason.starts_with("changed since the plan")
        || reason.starts_with("another file in this thread")
        || reason.starts_with("cannot ")
        || reason.contains("nothing was moved")
        || reason.starts_with("in use by")
        || reason.starts_with("activity is unknown")
}

struct Doc<'a> {
    started: i64,
    as_of: i64,
    older: &'a Span,
    codex: &'a Path,
    source: &'a str,
    plan: &'a str,
    expect: Option<&'a str>,
    files: &'a [FilePlan],
    purges: &'a [PurgePlan],
    errors: &'a [String],
    warnings: &'a [String],
}

fn document(doc: &Doc<'_>) -> String {
    let Doc {
        started,
        as_of,
        older,
        codex,
        source,
        plan,
        expect,
        files,
        purges,
        errors,
        warnings,
    } = doc;
    let results = files
        .iter()
        .map(|file| {
            Json::Obj(vec![
                ("path", Json::path(&file.path)),
                ("thread_id", Json::str(file.thread.clone())),
                ("bytes", Json::UInt(file.bytes)),
                (
                    "last_change",
                    Json::Obj(vec![
                        ("unix", Json::Int(file.mtime)),
                        ("at", Json::str(output::rfc3339(file.mtime))),
                    ]),
                ),
                ("result", Json::str(result_word(file))),
                (
                    "reason",
                    if file.reason.is_empty() {
                        Json::Null
                    } else {
                        Json::str(file.reason.clone())
                    },
                ),
            ])
        })
        .collect();
    let purged = purges
        .iter()
        .map(|purge| {
            Json::Obj(vec![
                ("entry", Json::str(purge.entry.clone())),
                ("entry_path", Json::path(&purge.dir)),
                ("result", Json::str(purge_word(purge))),
                (
                    "reason",
                    if purge.reason.is_empty() {
                        Json::Null
                    } else {
                        Json::str(purge.reason.clone())
                    },
                ),
            ])
        })
        .collect();
    Json::Obj(vec![
        ("schema", Json::str(SCHEMA)),
        ("version", Json::str(VERSION)),
        ("generated_at", Json::str(output::rfc3339(*started))),
        ("as_of", Json::str(output::rfc3339(*as_of))),
        ("older_than", Json::str(older.text.clone())),
        ("older_than_seconds", Json::Int(older.seconds)),
        ("codex_home", Json::path(codex)),
        ("codex_home_source", Json::str(*source)),
        ("plan", Json::str(*plan)),
        (
            "expected_plan",
            Json::opt(expect.map(|text| (*text).to_string()), Json::str),
        ),
        ("results", Json::Arr(results)),
        ("purged", Json::Arr(purged)),
        (
            "errors",
            Json::Arr(errors.iter().cloned().map(Json::str).collect()),
        ),
        (
            "warnings",
            Json::Arr(warnings.iter().cloned().map(Json::str).collect()),
        ),
    ])
    .render()
}

fn result_word(file: &FilePlan) -> &'static str {
    match file.act {
        Act::Trash if file.reason.is_empty() => "trashed",
        Act::Trash => "not_attempted",
        Act::Skip => {
            if file.reason.starts_with("stdin is not") || file.reason.starts_with("you did not") {
                "not_attempted"
            } else {
                "skipped"
            }
        }
        Act::Refuse => "refused",
    }
}

fn purge_word(purge: &PurgePlan) -> &'static str {
    if purge.reason.is_empty() && purge.act == Act::Trash {
        "purged"
    } else if purge.reason.starts_with("stdin is not") || purge.reason.starts_with("you did not") {
        "not_attempted"
    } else if purge.act == Act::Refuse {
        "refused"
    } else {
        "skipped"
    }
}

fn write_stdout(text: &str) -> bool {
    let mut out = io::stdout().lock();
    out.write_all(text.as_bytes()).is_ok() && out.write_all(b"\n").is_ok() && out.flush().is_ok()
}

fn list_entries() -> Result<Vec<Listed>, String> {
    let root = trash_root()?;
    let entries = match fs::read_dir(&root) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("{}: {e}", root.display())),
        Ok(entries) => entries,
    };
    let mut listed = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("{}: {e}", root.display()))?;
        let dir = entry.path();
        let meta = match fs::symlink_metadata(&dir) {
            Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => meta,
            _ => continue,
        };
        let _ = meta;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let info_path = dir.join("info");
        let info_bytes = fs::read(&info_path).ok();
        let id = fingerprint::entry("agent_files", &dir, info_bytes.as_deref());
        let record = match info_bytes {
            Some(bytes) => parse_record(&bytes),
            None => Err(format!("cannot read {}", info_path.display())),
        };
        listed.push(Listed {
            name,
            dir,
            id,
            record,
        });
    }
    listed.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(listed)
}

fn parse_record(bytes: &[u8]) -> Result<Record, String> {
    let mut codex_home = None;
    let mut trashed = None;
    let mut keep_seconds = None;
    let mut keep_text = None;
    let mut files = Vec::new();
    for line in bytes.split(|b| *b == b'\n') {
        if line.is_empty() || line.starts_with(b"#") {
            continue;
        }
        if let Some(rest) = line.strip_prefix(b"codex_home ") {
            codex_home = Some(PathBuf::from(OsString::from_vec(rest.to_vec())));
        } else if let Some(rest) = line.strip_prefix(b"trashed ") {
            let text = std::str::from_utf8(rest).map_err(|_| "trashed is not UTF-8")?;
            let seconds = text
                .split(' ')
                .next()
                .and_then(|s| s.parse::<i64>().ok())
                .ok_or("trashed has no time")?;
            trashed = Some(seconds);
        } else if let Some(rest) = line.strip_prefix(b"keep ") {
            let text = std::str::from_utf8(rest).map_err(|_| "keep is not UTF-8")?;
            let (seconds, shown) = text.split_once(' ').ok_or("keep has no duration")?;
            keep_seconds = Some(seconds.parse::<i64>().map_err(|_| "keep has no duration")?);
            keep_text = Some(shown.to_string());
        } else if let Some(rest) = line.strip_prefix(b"file ") {
            files.push(parse_file_line(rest)?);
        }
    }
    Ok(Record {
        codex_home: codex_home.ok_or("the record has no codex_home")?,
        trashed: trashed.ok_or("the record has no trashed time")?,
        keep_seconds: keep_seconds.ok_or("the record has no keep")?,
        keep_text: keep_text.ok_or("the record has no keep")?,
        files,
    })
}

fn parse_file_line(rest: &[u8]) -> Result<RecFile, String> {
    let mut nums = [0u64; 6];
    let mut cursor = rest;
    for slot in &mut nums {
        let (value, next) = take_u64(cursor).ok_or("a file line is short")?;
        *slot = value;
        cursor = next.strip_prefix(b" ").ok_or("a file line is short")?;
    }
    let thread_len = usize::try_from(nums[5]).map_err(|_| "a thread id is too long")?;
    if cursor.len() < thread_len + 1 {
        return Err("a file line is short".into());
    }
    let thread = std::str::from_utf8(&cursor[..thread_len])
        .map_err(|_| "a thread id is not UTF-8")?
        .to_string();
    cursor = &cursor[thread_len..];
    cursor = cursor.strip_prefix(b" ").ok_or("a file line is short")?;
    let (slot, path) = split_token(cursor).ok_or("a file line has no path")?;
    Ok(RecFile {
        path: PathBuf::from(OsString::from_vec(path.to_vec())),
        thread,
        id: Identity {
            dev: nums[0],
            ino: nums[1],
            size: nums[2],
            mtime: i64::try_from(nums[3]).unwrap_or(0),
            nsec: i64::try_from(nums[4]).unwrap_or(0),
        },
        slot: slot.to_string(),
    })
}

fn take_u64(bytes: &[u8]) -> Option<(u64, &[u8])> {
    let end = bytes.iter().position(|b| !b.is_ascii_digit())?;
    if end == 0 {
        return None;
    }
    let text = std::str::from_utf8(&bytes[..end]).ok()?;
    Some((text.parse().ok()?, &bytes[end..]))
}

fn split_token(bytes: &[u8]) -> Option<(&str, &[u8])> {
    let end = bytes.iter().position(|b| *b == b' ')?;
    let token = std::str::from_utf8(&bytes[..end]).ok()?;
    Some((token, &bytes[end + 1..]))
}

/// Rows for `residue restore --json`: every agent-file entry, and errors
/// reading the trash. The worktree list is unchanged.
pub fn restore_rows() -> (Vec<Json>, Vec<String>) {
    match list_entries() {
        Ok(entries) => (entries.iter().map(entry_json).collect(), Vec::new()),
        Err(message) => (Vec::new(), vec![message]),
    }
}

fn entry_json(entry: &Listed) -> Json {
    let record = entry.record.as_ref().ok();
    let file_json = record
        .map(|r| {
            r.files
                .iter()
                .map(|file| {
                    Json::Obj(vec![
                        ("path", Json::path(&file.path)),
                        ("thread_id", Json::str(file.thread.clone())),
                        ("bytes", Json::UInt(file.id.size)),
                    ])
                })
                .collect()
        })
        .unwrap_or_default();
    Json::Obj(vec![
        ("kind", Json::str("agent_files")),
        ("entry", Json::str(entry.name.clone())),
        ("entry_path", Json::path(&entry.dir)),
        ("id", Json::str(entry.id.clone())),
        (
            "trashed_at",
            Json::opt(record.map(|r| r.trashed), |t| Json::str(output::rfc3339(t))),
        ),
        (
            "keep",
            Json::opt(record.map(|r| r.keep_text.clone()), Json::str),
        ),
        (
            "keep_seconds",
            Json::opt(record.map(|r| r.keep_seconds), Json::Int),
        ),
        ("files", Json::Arr(file_json)),
        (
            "error",
            match &entry.record {
                Err(message) => Json::str(message.clone()),
                Ok(_) => Json::Null,
            },
        ),
    ])
}

pub fn restore_text() -> String {
    let Ok(entries) = list_entries() else {
        return String::new();
    };
    if entries.is_empty() {
        return String::new();
    }
    let mut out = String::from("\nCodex session logs in the trash\n");
    for entry in &entries {
        let when = entry
            .record
            .as_ref()
            .ok()
            .map(|r| output::rfc3339(r.trashed))
            .unwrap_or_else(|| "unreadable".into());
        let count = entry.record.as_ref().map(|r| r.files.len()).unwrap_or(0);
        out.push_str(&format!(
            "  {}  {}  {count} files  {}\n",
            entry.name, when, entry.id
        ));
    }
    out
}

/// Restore `ENTRY` when it names an agent-file trash entry. `None` when it
/// does not, so `residue restore` goes on to worktrees.
pub fn try_restore(options: &Options) -> Option<u8> {
    let wanted = options.paths.first()?;
    let entries = match list_entries() {
        Ok(entries) => entries,
        Err(_) => return None,
    };
    let index = find_entry(&entries, wanted)?;
    if options.to.is_some() {
        eprintln!(
            "residue: --to applies to a worktree; an agent-files entry goes back to the path it had"
        );
        return Some(2);
    }
    Some(restore_entry(options, &entries, index))
}

fn find_entry(entries: &[Listed], wanted: &Path) -> Option<usize> {
    let text = wanted.as_os_str();
    if let Some(index) = entries
        .iter()
        .position(|e| e.name.as_bytes() == text.as_bytes())
    {
        return Some(index);
    }
    if let Some(index) = entries.iter().position(|e| e.dir == wanted) {
        return Some(index);
    }
    let matches: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| {
            entry
                .record
                .as_ref()
                .ok()
                .is_some_and(|r| r.files.iter().any(|f| f.path == wanted))
        })
        .map(|(i, _)| i)
        .collect();
    match matches.as_slice() {
        [one] => Some(*one),
        _ => None,
    }
}

fn restore_entry(options: &Options, entries: &[Listed], index: usize) -> u8 {
    let entry = &entries[index];
    let expect = options.expect.as_deref();
    if expect.is_some_and(|want| want != entry.id) {
        println!(
            "nothing restored: {} now names the agent files {}, not the entry --expect names",
            entry.name, entry.id
        );
        println!(
            "next: if that is the one to restore, run again with --expect {}",
            entry.id
        );
        return fingerprint::CHANGED;
    }
    let info = entry.dir.join("info");
    let held = match hold::take(&info) {
        Ok(Taken::Held(held)) => held,
        Ok(Taken::Busy) => {
            return commands::fail(&format!(
                "cannot restore {}: another residue run holds it",
                entry.name
            ));
        }
        Ok(Taken::Gone) => {
            return commands::fail(&format!(
                "cannot restore {}: its record is gone",
                entry.name
            ));
        }
        Ok(Taken::Unsupported(why)) => return commands::fail(&why),
        Err(message) => return commands::fail(&message),
    };
    let now_id = match fingerprint::entry_now("agent_files", &entry.dir, &info) {
        Ok(id) => id,
        Err(message) => return commands::fail(&message),
    };
    if expect.is_some_and(|want| want != now_id) {
        drop(held);
        println!(
            "nothing restored: {} changed after it was listed, so nothing was restored",
            entry.name
        );
        println!("next: if that is the one to restore, run again with --expect {now_id}");
        return fingerprint::CHANGED;
    }
    let record = match &entry.record {
        Ok(record) => record,
        Err(message) => return commands::fail(message),
    };
    let sessions = record.codex_home.join("sessions");
    let mut notes = Vec::new();
    let mut restored = 0usize;
    for file in &record.files {
        if !file.path.starts_with(&sessions) {
            notes.push(format!(
                "{} is not under {}, so it stays in the trash",
                file.path.display(),
                sessions.display()
            ));
            continue;
        }
        if fs::symlink_metadata(&file.path).is_ok() {
            notes.push(format!(
                "{} is already there, so the trash copy stays",
                file.path.display()
            ));
            continue;
        }
        if let Err(message) = ensure_parent(&sessions, &file.path) {
            notes.push(format!("{}: {message}", file.path.display()));
            continue;
        }
        let src = entry.dir.join("files").join(&file.slot);
        match rename_matching(&src, &file.path, &file.id) {
            Ok(()) => restored += 1,
            Err(message) => notes.push(format!("{}: {message}", file.path.display())),
        }
    }
    if restored == record.files.len() {
        remove_empty_entry(&entry.dir);
    }
    if let Err(message) = write_ledger(
        Event::Restored,
        Some("agent_files"),
        0,
        &record.codex_home,
        &entry.name,
    ) {
        notes.push(format!(
            "the stats ledger is not up to date ({message}); this does not change the restore"
        ));
    }
    for note in &notes {
        eprintln!("residue: note: {}", clean(note));
    }
    if options.json {
        println!(
            "{}",
            restore_document(entry, restored, &notes, expect).render()
        );
    } else if restored == 0 {
        println!("nothing restored: {}", notes.join("; "));
    } else {
        println!(
            "restored {restored} Codex session {} from {}",
            if restored == 1 { "file" } else { "files" },
            entry.name
        );
    }
    if restored == 0 { 3 } else { 0 }
}

fn remove_empty_entry(dir: &Path) {
    let files = dir.join("files");
    let _ = fs::remove_dir(&files);
    let _ = fs::remove_file(dir.join("info"));
    let _ = fs::remove_dir(dir);
}

fn ensure_parent(sessions: &Path, file: &Path) -> Result<(), String> {
    let parent = file
        .parent()
        .filter(|p| p.starts_with(sessions))
        .ok_or_else(|| "its parent is not under sessions".to_string())?;
    let relative = parent
        .strip_prefix(sessions)
        .map_err(|_| "its parent is not under sessions".to_string())?;
    let mut current = sessions.to_path_buf();
    if let Ok(meta) = fs::symlink_metadata(sessions) {
        if meta.file_type().is_symlink() || !meta.is_dir() {
            return Err(format!(
                "{} is not a directory residue can write",
                sessions.display()
            ));
        }
    }
    for component in relative.components() {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(format!("{} is a symbolic link", current.display()));
            }
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => return Err(format!("{} is not a directory", current.display())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                fs::DirBuilder::new()
                    .mode(0o700)
                    .create(&current)
                    .map_err(|err| format!("{}: {err}", current.display()))?;
            }
            Err(e) => return Err(format!("{}: {e}", current.display())),
        }
    }
    Ok(())
}

fn restore_document(
    entry: &Listed,
    restored: usize,
    notes: &[String],
    expect: Option<&str>,
) -> Json {
    let path = entry
        .record
        .as_ref()
        .ok()
        .and_then(|r| r.files.first())
        .map(|f| f.path.as_path());
    Json::Obj(vec![
        ("schema", Json::str(output::SCHEMA_RESTORE)),
        ("version", Json::str(VERSION)),
        ("generated_at", Json::str(output::rfc3339(output::now()))),
        (
            "restored",
            if restored == 0 {
                Json::Null
            } else {
                Json::opt(path, Json::path)
            },
        ),
        (
            "restore",
            Json::Obj(vec![
                ("kind", Json::str("agent_files")),
                ("entry", Json::str(entry.name.clone())),
                ("path", Json::opt(path, Json::path)),
                ("files_restored", Json::UInt(restored as u64)),
                (
                    "notes",
                    Json::Arr(notes.iter().cloned().map(Json::str).collect()),
                ),
            ]),
        ),
        ("trash", Json::Arr(Vec::new())),
        ("removed", Json::Arr(Vec::new())),
        ("agent_files", Json::Arr(vec![entry_json(entry)])),
        ("errors", Json::Arr(Vec::new())),
        (
            "expected_id",
            Json::opt(expect.map(str::to_string), Json::str),
        ),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_uuid_groups_the_parts_of_one_thread() {
        let one = OsStr::from_bytes(
            b"rollout-2020-01-01T00-00-00-11111111-1111-1111-1111-111111111111.jsonl",
        );
        let two = OsStr::from_bytes(
            b"rollout-2026-11-01T00-00-00-11111111-1111-1111-1111-111111111111_2.jsonl.zst",
        );
        let other = OsStr::from_bytes(
            b"rollout-2020-01-01T00-00-00-22222222-2222-2222-2222-222222222222.jsonl",
        );
        assert_eq!(thread_id(one), thread_id(two));
        assert_ne!(thread_id(one), thread_id(other));
        assert!(is_rollout(one));
        assert!(!is_rollout(OsStr::new("history.jsonl")));
        assert!(!is_rollout(OsStr::new("notes.txt")));
    }

    #[test]
    fn a_file_line_round_trips() {
        let line = b"file 1 2 3 4 5 3 abc 0 /tmp/rollout.jsonl";
        let parsed = parse_file_line(line.strip_prefix(b"file ").unwrap()).unwrap();
        assert_eq!(parsed.thread, "abc");
        assert_eq!(parsed.slot, "0");
        assert_eq!(parsed.path, PathBuf::from("/tmp/rollout.jsonl"));
        assert_eq!(parsed.id.dev, 1);
        assert_eq!(parsed.id.size, 3);
    }
}
