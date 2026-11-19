//! `residue agents`: what Codex and Claude Code keep on disk.
//!
//! Read-only. It never writes, the stats ledger included, and it never
//! removes or moves a file. `watch` and `service` do not run it.

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::activity::{Evidence, Snapshot};
use crate::cli::Options;
use crate::fsinfo::{self, Usage};
use crate::inspect::{self, Kept, Verdict};
use crate::json::Json;
use crate::output::{self, Style};

const SCHEMA: &str = "residue.agents/v1";

const CODEX_ROLLOUT: &str = "https://github.com/openai/codex/blob/b8cbdbfeff72869e5d715480572aad24f739aa65/codex-rs/rollout/src/lib.rs";
const CODEX_STATE: &str = "https://github.com/openai/codex/blob/9e77fb4ad5ff4975f25c4d1d121aece3c40c5176/codex-rs/state/src/sqlite.rs";
const CODEX_CONFIG: &str = "https://developers.openai.com/codex/config-reference";
const CODEX_DELETE: &str = "https://developers.openai.com/codex/cli/reference";
const CLAUDE_DIR: &str = "https://code.claude.com/docs/en/claude-directory#application-data";
const CLAUDE_SCRATCH: &str =
    "https://code.claude.com/docs/en/claude-directory#session-scratchpad-directory";
const CLAUDE_CLEANUP: &str = "https://code.claude.com/docs/en/settings-reference#cleanupperioddays";
const CLAUDE_DESKTOP: &str =
    "https://code.claude.com/docs/en/settings-reference#desktopsessioncleanupperioddays";
const CLAUDE_PURGE: &str = "https://code.claude.com/docs/en/claude-directory#clear-local-data";

struct Cleanup {
    what: &'static str,
    doc: &'static str,
}

const CODEX_OWN: &[Cleanup] = &[
    Cleanup {
        what: "history.max_bytes caps history.jsonl only; rollouts have no age limit",
        doc: CODEX_CONFIG,
    },
    Cleanup {
        what: "/delete removes the current session; codex delete SESSION removes one by id",
        doc: CODEX_DELETE,
    },
];

const CLAUDE_OWN: &[Cleanup] = &[
    Cleanup {
        what: "cleanupPeriodDays deletes transcripts after 30 days by default",
        doc: CLAUDE_CLEANUP,
    },
    Cleanup {
        what: "desktopSessionCleanupPeriodDays ages Desktop and Cowork transcripts; unset keeps them",
        doc: CLAUDE_DESKTOP,
    },
    Cleanup {
        what: "claude purge PATH --dry-run shows one project's plan",
        doc: CLAUDE_PURGE,
    },
];

struct StoreSpec {
    store: &'static str,
    /// Name under the agent root. `None` for the scratch directory, which
    /// lives under the system temp directory.
    rel: Option<&'static str>,
    kind: SpecKind,
    source_url: &'static str,
}

#[derive(Clone, Copy)]
enum SpecKind {
    /// A directory. A missing one is `missing`, never 0 bytes.
    Dir,
    /// One file. A symbolic link is not followed.
    File,
    /// `*.sqlite` directly in the agent root, not in a subdirectory.
    /// None there is `missing`: `sqlite_home` may have moved them, and
    /// residue does not read `config.toml` to find out.
    Sqlite,
    Scratch,
}

const CODEX_STORES: &[StoreSpec] = &[
    StoreSpec {
        store: "sessions",
        rel: Some("sessions"),
        kind: SpecKind::Dir,
        source_url: CODEX_ROLLOUT,
    },
    StoreSpec {
        store: "archived_sessions",
        rel: Some("archived_sessions"),
        kind: SpecKind::Dir,
        source_url: CODEX_ROLLOUT,
    },
    StoreSpec {
        store: "history",
        rel: Some("history.jsonl"),
        kind: SpecKind::File,
        source_url: CODEX_CONFIG,
    },
    StoreSpec {
        store: "logs",
        rel: Some("log"),
        kind: SpecKind::Dir,
        source_url: CODEX_CONFIG,
    },
    StoreSpec {
        store: "state_db",
        rel: None,
        kind: SpecKind::Sqlite,
        source_url: CODEX_STATE,
    },
];

const CLAUDE_STORES: &[StoreSpec] = &[
    StoreSpec {
        store: "projects",
        rel: Some("projects"),
        kind: SpecKind::Dir,
        source_url: CLAUDE_DIR,
    },
    StoreSpec {
        store: "file_history",
        rel: Some("file-history"),
        kind: SpecKind::Dir,
        source_url: CLAUDE_DIR,
    },
    StoreSpec {
        store: "plans",
        rel: Some("plans"),
        kind: SpecKind::Dir,
        source_url: CLAUDE_DIR,
    },
    StoreSpec {
        store: "debug",
        rel: Some("debug"),
        kind: SpecKind::Dir,
        source_url: CLAUDE_DIR,
    },
    StoreSpec {
        store: "paste_cache",
        rel: Some("paste-cache"),
        kind: SpecKind::Dir,
        source_url: CLAUDE_DIR,
    },
    StoreSpec {
        store: "shell_snapshots",
        rel: Some("shell-snapshots"),
        kind: SpecKind::Dir,
        source_url: CLAUDE_DIR,
    },
    StoreSpec {
        store: "tasks",
        rel: Some("tasks"),
        kind: SpecKind::Dir,
        source_url: CLAUDE_DIR,
    },
    StoreSpec {
        store: "scratch",
        rel: None,
        kind: SpecKind::Scratch,
        source_url: CLAUDE_SCRATCH,
    },
];

struct Store {
    store: &'static str,
    path: PathBuf,
    verified: bool,
    source_url: &'static str,
    state: &'static str,
    link_target: Option<PathBuf>,
    files: Option<u64>,
    apparent: Option<u64>,
    allocated: Option<u64>,
    last_unix: Option<i64>,
    last_what: Option<String>,
    activity: Option<Evidence>,
    errors: Vec<String>,
}

struct AgentKind {
    agent: &'static str,
    stores: &'static [StoreSpec],
    cleanup: &'static [Cleanup],
}

struct Agent {
    agent: &'static str,
    root: PathBuf,
    root_source: &'static str,
    /// `root` itself is a symbolic link. Nothing under it is measured.
    root_linked: bool,
    link_target: Option<PathBuf>,
    stores: Vec<Store>,
    worktrees: Vec<inspect::Report>,
    cleanup: &'static [Cleanup],
}

struct Report {
    agents: Vec<Agent>,
    errors: Vec<String>,
    warnings: Vec<String>,
}

struct Request<'a> {
    home: &'a Path,
    /// `CODEX_HOME` when it is set. Otherwise `~/.codex`.
    codex_home: Option<&'a Path>,
    claude_config: Option<&'a Path>,
    /// `CLAUDE_CODE_TMPDIR` when it is set.
    claude_tmpdir: Option<&'a Path>,
    /// `TMPDIR` on Linux, where the scratch directory follows it.
    tmpdir: Option<&'a Path>,
    roots: &'a [PathBuf],
    snapshot: &'a Snapshot,
}

pub fn command(options: &Options) -> u8 {
    let home = match home_dir() {
        Ok(path) => path,
        Err(message) => return crate::commands::fail(&message),
    };
    let codex = env_path("CODEX_HOME");
    let claude = env_path("CLAUDE_CONFIG_DIR");
    let claude_tmp = env_path("CLAUDE_CODE_TMPDIR");
    let tmp = env_path("TMPDIR");
    let snapshot = Snapshot::take();
    let now = output::now();
    let report = build(Request {
        home: &home,
        codex_home: codex.as_deref(),
        claude_config: claude.as_deref(),
        claude_tmpdir: claude_tmp.as_deref(),
        tmpdir: tmp.as_deref(),
        roots: &options.paths,
        snapshot: &snapshot,
    });
    let body = if options.json {
        report.json(&snapshot, now)
    } else {
        report.text(&snapshot, now)
    };
    if write_stdout(&body) { 0 } else { 1 }
}

fn home_dir() -> Result<PathBuf, String> {
    match std::env::var_os("HOME") {
        Some(value) if !value.is_empty() && value != "/" => Ok(PathBuf::from(value)),
        _ => Err("HOME is unset or /, so residue cannot find an agent's directory".into()),
    }
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn write_stdout(text: &str) -> bool {
    let mut out = io::stdout().lock();
    out.write_all(text.as_bytes()).is_ok() && out.write_all(b"\n").is_ok() && out.flush().is_ok()
}

fn build(req: Request<'_>) -> Report {
    let mut warnings = Vec::new();
    let mut errors = Vec::new();
    let codex_root = req
        .codex_home
        .map(Path::to_path_buf)
        .unwrap_or_else(|| req.home.join(".codex"));
    let codex_source = if req.codex_home.is_some() {
        "CODEX_HOME"
    } else {
        "default"
    };
    let claude_root = req
        .claude_config
        .map(Path::to_path_buf)
        .unwrap_or_else(|| req.home.join(".claude"));
    let claude_source = if req.claude_config.is_some() {
        "CLAUDE_CONFIG_DIR"
    } else {
        "default"
    };

    let codex_open = root_open(&codex_root);
    let claude_open = root_open(&claude_root);
    let (codex_worktrees, worktree_note) = match &codex_open {
        RootOpen::Directory => worktree_dir(&codex_root),
        _ => (None, None),
    };
    if let Some(note) = worktree_note {
        warnings.push(note);
    }

    let mut scan_roots = req.roots.to_vec();
    if let Some(dir) = &codex_worktrees {
        scan_roots.push(dir.clone());
    }
    let (codex_trees, claude_trees, scan_errors) =
        judge_worktrees(&scan_roots, codex_worktrees.as_deref(), req.snapshot);
    errors.extend(scan_errors);

    let scratch = scratch_dir(req.claude_tmpdir, req.tmpdir);
    let codex = agent(
        &AgentKind {
            agent: "codex",
            stores: CODEX_STORES,
            cleanup: CODEX_OWN,
        },
        codex_root,
        codex_source,
        codex_open,
        &scratch,
        codex_trees,
        req.snapshot,
    );
    let claude = agent(
        &AgentKind {
            agent: "claude",
            stores: CLAUDE_STORES,
            cleanup: CLAUDE_OWN,
        },
        claude_root,
        claude_source,
        claude_open,
        &scratch,
        claude_trees,
        req.snapshot,
    );
    // Cursor's worktree directory is not named in Cursor's docs
    // (https://cursor.com/docs/configuration/worktrees), so it is left out
    // rather than reported from a guessed path (#232). Codex worktrees are
    // `$CODEX_HOME/worktrees` (https://learn.chatgpt.com/docs/environments/git-worktrees)
    // and Claude's are `<repo>/.claude/worktrees/`
    // (https://code.claude.com/docs/en/worktrees).
    Report {
        agents: vec![codex, claude],
        errors,
        warnings,
    }
}

enum RootOpen {
    Missing,
    Symlink(Option<PathBuf>),
    Unreadable(String),
    NotDirectory,
    Directory,
}

fn root_open(path: &Path) -> RootOpen {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => RootOpen::Missing,
        Err(e) => RootOpen::Unreadable(format!("{}: {e}", path.display())),
        Ok(meta) if meta.file_type().is_symlink() => RootOpen::Symlink(fs::read_link(path).ok()),
        Ok(meta) if meta.is_dir() => RootOpen::Directory,
        Ok(_) => RootOpen::NotDirectory,
    }
}

/// `$CODEX_HOME/worktrees` when it is a real directory. A symbolic link is
/// not followed: the warning says so.
fn worktree_dir(codex: &Path) -> (Option<PathBuf>, Option<String>) {
    let path = codex.join("worktrees");
    match fs::symlink_metadata(&path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => (None, None),
        Err(e) => (
            None,
            Some(format!("could not read {}: {e}", path.display())),
        ),
        Ok(meta) if meta.file_type().is_symlink() => (
            None,
            Some(format!(
                "{} is a symbolic link, so its worktrees were not listed",
                path.display()
            )),
        ),
        Ok(meta) if !meta.is_dir() => (
            None,
            Some(format!(
                "{} is not a directory, so its worktrees were not listed",
                path.display()
            )),
        ),
        Ok(_) => match crate::git::canonical(&path) {
            Ok(canonical) => (Some(canonical), None),
            Err(e) => (None, Some(e)),
        },
    }
}

fn judge_worktrees(
    roots: &[PathBuf],
    codex_root: Option<&Path>,
    snapshot: &Snapshot,
) -> (Vec<inspect::Report>, Vec<inspect::Report>, Vec<String>) {
    if roots.is_empty() {
        return (Vec::new(), Vec::new(), Vec::new());
    }
    let policy = match crate::commands::prepare(&Options::default()) {
        Ok(policy) => policy,
        Err(e) => return (Vec::new(), Vec::new(), vec![e]),
    };
    let found = crate::discover::find(roots);
    let mut errors = found.errors;
    let kept: Vec<Kept> = found
        .repos
        .iter()
        .map(|repo| Kept::of(&repo.main, &repo.common_dir))
        .collect();
    let jobs: Vec<inspect::Job> = found
        .repos
        .iter()
        .zip(&kept)
        .flat_map(|(repo, kept)| {
            repo.linked
                .iter()
                .map(move |entry| (entry, repo.common_dir.as_path(), kept))
        })
        .collect();
    let mut reports = inspect::all(&jobs, snapshot, &policy);
    let mut codex = Vec::new();
    let mut claude = Vec::new();
    for repo in &found.repos {
        let n = repo.linked.len();
        if reports.len() < n {
            errors.push(format!(
                "{}: judged {} of {} linked worktrees",
                repo.common_dir.display(),
                reports.len(),
                n
            ));
            break;
        }
        for report in reports.drain(0..n) {
            let path = report.canonical.as_deref().unwrap_or(&report.entry.path);
            match owner(path, codex_root) {
                Some("codex") => codex.push(report),
                Some("claude") => claude.push(report),
                _ => {}
            }
        }
    }
    (codex, claude, errors)
}

/// `codex` when `path` is under Codex's worktree root, `claude` when a
/// `.claude/worktrees` directory contains it, and neither otherwise.
fn owner(path: &Path, codex_root: Option<&Path>) -> Option<&'static str> {
    if codex_root.is_some_and(|root| path.starts_with(root)) {
        return Some("codex");
    }
    let mut previous = None;
    for component in path.components() {
        if previous.is_some_and(|name| name == ".claude") && component.as_os_str() == "worktrees" {
            return Some("claude");
        }
        previous = Some(component.as_os_str());
    }
    None
}

fn scratch_dir(claude_tmpdir: Option<&Path>, tmpdir: Option<&Path>) -> Result<PathBuf, String> {
    // SAFETY: geteuid only returns this process's effective user.
    let uid = unsafe { libc::geteuid() };
    let name = format!("claude-{uid}");
    if let Some(root) = claude_tmpdir {
        if !root.is_absolute() {
            return Err("CLAUDE_CODE_TMPDIR is not an absolute path".into());
        }
        return Ok(root.join(name));
    }
    Ok(default_scratch(&name, tmpdir))
}

#[cfg(target_os = "macos")]
fn default_scratch(name: &str, _tmpdir: Option<&Path>) -> PathBuf {
    PathBuf::from("/private/tmp").join(name)
}

#[cfg(not(target_os = "macos"))]
fn default_scratch(name: &str, tmpdir: Option<&Path>) -> PathBuf {
    if let Some(root) = tmpdir.filter(|path| path.is_absolute()) {
        root.join(name)
    } else {
        PathBuf::from("/tmp").join(name)
    }
}

fn agent(
    kind: &AgentKind,
    root: PathBuf,
    root_source: &'static str,
    open: RootOpen,
    scratch: &Result<PathBuf, String>,
    worktrees: Vec<inspect::Report>,
    snapshot: &Snapshot,
) -> Agent {
    let (root_linked, link_target) = match &open {
        RootOpen::Symlink(target) => (true, target.clone()),
        _ => (false, None),
    };
    let wanted = snapshot.wanted();
    let stores = kind
        .stores
        .iter()
        .map(|spec| measure_spec(spec, &root, &open, scratch, snapshot, &wanted))
        .collect();
    Agent {
        agent: kind.agent,
        root,
        root_source,
        root_linked,
        link_target,
        stores,
        worktrees,
        cleanup: kind.cleanup,
    }
}

fn measure_spec(
    spec: &StoreSpec,
    root: &Path,
    open: &RootOpen,
    scratch: &Result<PathBuf, String>,
    snapshot: &Snapshot,
    wanted: &[(u64, u64)],
) -> Store {
    if matches!(spec.kind, SpecKind::Scratch) {
        return match scratch {
            Ok(path) => measure_dir_store(spec, path, snapshot, wanted),
            Err(message) => bare(
                spec,
                root.join("scratch"),
                "unreadable",
                vec![message.clone()],
            ),
        };
    }
    let path = match (spec.kind, spec.rel) {
        (SpecKind::Sqlite, _) => root.to_path_buf(),
        (SpecKind::Dir | SpecKind::File, Some(rel)) => root.join(rel),
        (SpecKind::Scratch, _) | (_, None) => {
            return bare(
                spec,
                root.to_path_buf(),
                "unreadable",
                vec![format!("{} has no path", spec.store)],
            );
        }
    };
    match open {
        RootOpen::Missing => bare(spec, path, "missing", Vec::new()),
        RootOpen::Symlink(target) => Store {
            link_target: target.clone(),
            ..bare(spec, path, "symlink", Vec::new())
        },
        RootOpen::Unreadable(message) => bare(spec, path, "unreadable", vec![message.clone()]),
        RootOpen::NotDirectory => bare(
            spec,
            path,
            "unreadable",
            vec![format!("{} is not a directory", root.display())],
        ),
        RootOpen::Directory => match spec.kind {
            SpecKind::Dir => measure_dir_store(spec, &path, snapshot, wanted),
            SpecKind::File => measure_file_store(spec, &path, snapshot),
            SpecKind::Sqlite => measure_sqlite(spec, root, snapshot),
            SpecKind::Scratch => bare(spec, path, "unreadable", Vec::new()),
        },
    }
}

fn bare(spec: &StoreSpec, path: PathBuf, state: &'static str, errors: Vec<String>) -> Store {
    Store {
        store: spec.store,
        path,
        verified: true,
        source_url: spec.source_url,
        state,
        link_target: None,
        files: None,
        apparent: None,
        allocated: None,
        last_unix: None,
        last_what: None,
        activity: None,
        errors,
    }
}

fn measure_dir_store(
    spec: &StoreSpec,
    path: &Path,
    snapshot: &Snapshot,
    wanted: &[(u64, u64)],
) -> Store {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            bare(spec, path.to_path_buf(), "missing", Vec::new())
        }
        Err(e) => bare(
            spec,
            path.to_path_buf(),
            "unreadable",
            vec![format!("{}: {e}", path.display())],
        ),
        Ok(meta) if meta.file_type().is_symlink() => Store {
            link_target: fs::read_link(path).ok(),
            ..bare(spec, path.to_path_buf(), "symlink", Vec::new())
        },
        Ok(meta) if !meta.is_dir() => bare(
            spec,
            path.to_path_buf(),
            "unreadable",
            vec![format!("{} is not a directory", path.display())],
        ),
        Ok(_) => match crate::git::canonical(path) {
            Err(e) => bare(spec, path.to_path_buf(), "unreadable", vec![e]),
            Ok(canonical) => match fsinfo::measure(&canonical, &[], &|_| false, &|_| false, wanted)
            {
                Err(e) => bare(
                    spec,
                    path.to_path_buf(),
                    "unreadable",
                    vec![format!("{}: {e}", path.display())],
                ),
                Ok(usage) => dir_result(spec, path, &canonical, usage, snapshot),
            },
        },
    }
}

fn dir_result(
    spec: &StoreSpec,
    path: &Path,
    canonical: &Path,
    usage: Usage,
    snapshot: &Snapshot,
) -> Store {
    let mut errors = Vec::new();
    if let Some(first) = &usage.first_error {
        errors.push(first.clone());
    }
    for mount in &usage.mount_points {
        errors.push(format!("{} is another mount, not entered", mount.display()));
    }
    if let Some(why) = &usage.mounts_unknown {
        errors.push(format!("could not tell mount points: {why}"));
    }
    let partial =
        usage.unreadable > 0 || !usage.mount_points.is_empty() || usage.mounts_unknown.is_some();
    let what = usage
        .newest_path
        .as_ref()
        .map(|newest| relative(canonical, newest));
    Store {
        store: spec.store,
        path: path.to_path_buf(),
        verified: true,
        source_url: spec.source_url,
        state: if partial { "partial" } else { "ok" },
        link_target: None,
        files: Some(usage.files),
        apparent: Some(usage.apparent),
        allocated: Some(usage.allocated),
        last_unix: usage.newest_mtime,
        last_what: what,
        activity: Some(snapshot.evidence(canonical, Some(&usage.reach))),
        errors,
    }
}

fn relative(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    if rel.as_os_str().is_empty() {
        ".".to_string()
    } else {
        rel.to_string_lossy().into_owned()
    }
}

fn measure_file_store(spec: &StoreSpec, path: &Path, snapshot: &Snapshot) -> Store {
    match file_stat(path) {
        FileStat::Missing => bare(spec, path.to_path_buf(), "missing", Vec::new()),
        FileStat::Unreadable(message) => {
            bare(spec, path.to_path_buf(), "unreadable", vec![message])
        }
        FileStat::Symlink(target) => Store {
            link_target: target,
            ..bare(spec, path.to_path_buf(), "symlink", Vec::new())
        },
        FileStat::NotFile => bare(
            spec,
            path.to_path_buf(),
            "unreadable",
            vec![format!("{} is not a file", path.display())],
        ),
        FileStat::File {
            apparent,
            allocated,
            mtime,
        } => {
            let what = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned());
            Store {
                store: spec.store,
                path: path.to_path_buf(),
                verified: true,
                source_url: spec.source_url,
                state: "ok",
                link_target: None,
                files: Some(1),
                apparent: Some(apparent),
                allocated: Some(allocated),
                last_unix: Some(mtime),
                last_what: what,
                activity: Some(snapshot.evidence(path, None)),
                errors: Vec::new(),
            }
        }
    }
}

enum FileStat {
    Missing,
    Unreadable(String),
    Symlink(Option<PathBuf>),
    NotFile,
    File {
        apparent: u64,
        allocated: u64,
        mtime: i64,
    },
}

/// `lstat`. Never opens the file, so a transcript is not read.
fn file_stat(path: &Path) -> FileStat {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => FileStat::Missing,
        Err(e) => FileStat::Unreadable(format!("{}: {e}", path.display())),
        Ok(meta) if meta.file_type().is_symlink() => FileStat::Symlink(fs::read_link(path).ok()),
        Ok(meta) if !meta.is_file() => FileStat::NotFile,
        Ok(meta) => FileStat::File {
            apparent: meta.len(),
            allocated: meta.blocks().saturating_mul(512),
            mtime: meta.mtime(),
        },
    }
}

fn measure_sqlite(spec: &StoreSpec, root: &Path, snapshot: &Snapshot) -> Store {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) => {
            return bare(
                spec,
                root.to_path_buf(),
                "unreadable",
                vec![format!("{}: {e}", root.display())],
            );
        }
    };
    let mut files = Vec::new();
    let mut errors = Vec::new();
    let mut saw_name = false;
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                errors.push(format!("{}: {e}", root.display()));
                continue;
            }
        };
        let name = entry.file_name();
        if !name.to_string_lossy().ends_with(".sqlite") {
            continue;
        }
        saw_name = true;
        let path = entry.path();
        match fs::symlink_metadata(&path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => errors.push(format!("{}: {e}", path.display())),
            Ok(meta) if meta.file_type().is_symlink() => {
                errors.push(format!(
                    "{} is a symbolic link, not measured",
                    path.display()
                ));
            }
            Ok(meta) if meta.is_file() => files.push((path, meta)),
            Ok(_) => errors.push(format!("{} is not a file", path.display())),
        }
    }
    if !saw_name && errors.is_empty() {
        return bare(spec, root.to_path_buf(), "missing", Vec::new());
    }
    if files.is_empty() {
        let state = if errors.is_empty() {
            "missing"
        } else {
            "partial"
        };
        return Store {
            state,
            errors,
            ..bare(spec, root.to_path_buf(), "partial", Vec::new())
        };
    }
    let mut seen = BTreeSet::new();
    let mut apparent = 0u64;
    let mut allocated = 0u64;
    let mut newest: Option<(i64, String)> = None;
    let mut activity = Vec::new();
    for (path, meta) in &files {
        let id = (meta.dev(), meta.ino());
        if seen.insert(id) {
            apparent += meta.len();
            allocated += meta.blocks().saturating_mul(512);
        }
        let mtime = meta.mtime();
        let what = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        if newest.as_ref().is_none_or(|(at, _)| mtime >= *at) {
            newest = Some((mtime, what));
        }
        activity.push(snapshot.evidence(path, None));
    }
    let (last_unix, last_what) = match newest {
        Some((unix, what)) => (Some(unix), Some(what)),
        None => (None, None),
    };
    Store {
        store: spec.store,
        path: root.to_path_buf(),
        verified: true,
        source_url: spec.source_url,
        state: if errors.is_empty() { "ok" } else { "partial" },
        link_target: None,
        files: Some(files.len() as u64),
        apparent: Some(apparent),
        allocated: Some(allocated),
        last_unix,
        last_what,
        activity: Some(merge_activity(activity)),
        errors,
    }
}

fn merge_activity(parts: Vec<Evidence>) -> Evidence {
    let mut hits = Vec::new();
    let mut unknown = None;
    for part in parts {
        match part {
            Evidence::InUse(procs) => hits.extend(procs),
            Evidence::Unknown(why) if unknown.is_none() => unknown = Some(why),
            _ => {}
        }
    }
    if !hits.is_empty() {
        hits.sort();
        hits.dedup();
        return Evidence::InUse(hits);
    }
    unknown.map_or(Evidence::NotObserved, Evidence::Unknown)
}

impl Report {
    fn json(&self, snapshot: &Snapshot, now: i64) -> String {
        let agents = self.agents.iter().map(|agent| agent.json()).collect();
        Json::Obj(vec![
            ("schema", Json::str(SCHEMA)),
            ("version", Json::str(crate::commands::VERSION)),
            ("generated_at", Json::str(output::rfc3339(now))),
            ("activity", output::activity_json(snapshot)),
            ("agents", Json::Arr(agents)),
            ("errors", strings(&self.errors)),
            ("warnings", strings(&self.warnings)),
        ])
        .render()
    }

    fn text(&self, snapshot: &Snapshot, now: i64) -> String {
        let style = Style::stdout();
        let mut out = style.dim(&format!(
            "{} · activity: {}\n",
            crate::commands::header(now),
            output::clean(&snapshot.scope())
        ));
        let mut shown = false;
        for agent in &self.agents {
            if let Some(block) = agent.text(&style, now) {
                shown = true;
                out.push('\n');
                out.push_str(&block);
            }
        }
        if !shown {
            out.push_str("\nno Codex or Claude Code store found\n");
        }
        if !self.errors.is_empty() {
            out.push_str("\ncould not inspect:\n");
            for error in &self.errors {
                out.push_str(&format!("  {}\n", output::clean(error)));
            }
        }
        for warning in &self.warnings {
            out.push_str(&format!("\nwarning: {}\n", output::clean(warning)));
        }
        out
    }
}

fn strings(values: &[String]) -> Json {
    Json::Arr(values.iter().cloned().map(Json::str).collect())
}

impl Agent {
    fn json(&self) -> Json {
        let stores = self.stores.iter().map(Store::json).collect();
        let worktrees = self
            .worktrees
            .iter()
            .map(|report| output::agent_worktree_json(report, self.agent))
            .collect();
        let cleanup = self
            .cleanup
            .iter()
            .map(|item| {
                Json::Obj(vec![
                    ("what", Json::str(item.what)),
                    ("doc", Json::str(item.doc)),
                ])
            })
            .collect();
        Json::Obj(vec![
            ("agent", Json::str(self.agent)),
            ("root", Json::path(&self.root)),
            ("root_source", Json::str(self.root_source)),
            (
                "link_target",
                Json::opt(self.link_target.as_deref(), Json::path),
            ),
            ("stores", Json::Arr(stores)),
            ("worktrees", Json::Arr(worktrees)),
            ("own_cleanup", Json::Arr(cleanup)),
        ])
    }

    /// `None` when every store is missing and there is no worktree: the
    /// text leaves that agent out. The JSON still has it.
    fn text(&self, style: &Style, now: i64) -> Option<String> {
        let present =
            self.stores.iter().any(|store| store.state != "missing") || !self.worktrees.is_empty();
        if !present {
            return None;
        }
        let mut out = format!(
            "{} · {} · {}\n",
            style.bold(self.agent),
            output::home(&self.root.display().to_string()),
            self.heading()
        );
        for store in &self.stores {
            if let Some(line) = store.line(now) {
                out.push_str(&line);
            }
        }
        if !self.worktrees.is_empty() {
            out.push_str(&worktree_line(&self.worktrees));
        }
        let notes: Vec<&str> = self.cleanup.iter().map(|item| item.what).collect();
        out.push_str(&format!("  own cleanup: {}\n", notes.join("; ")));
        Some(out)
    }

    fn heading(&self) -> String {
        if self.root_linked {
            return match &self.link_target {
                Some(target) => format!(
                    "symbolic link to {}, not measured",
                    output::home(&target.display().to_string())
                ),
                None => "symbolic link, not measured".to_string(),
            };
        }
        if self.stores.iter().all(|store| store.state == "unreadable") && self.worktrees.is_empty()
        {
            let why = self
                .stores
                .first()
                .and_then(|store| store.errors.first())
                .map(|error| output::clean(error))
                .unwrap_or_else(|| "not read".to_string());
            return format!("not read: {why}");
        }
        let complete: u64 = self
            .stores
            .iter()
            .filter(|store| store.state == "ok")
            .filter_map(|store| store.allocated)
            .sum();
        let partial = self.stores.iter().any(|store| store.state == "partial");
        if partial && complete == 0 {
            "partial".to_string()
        } else if partial {
            format!("{}, plus partial stores", output::bytes(complete))
        } else if complete == 0 && !self.worktrees.is_empty() {
            "stores missing".to_string()
        } else {
            output::bytes(complete)
        }
    }
}

fn worktree_line(worktrees: &[inspect::Report]) -> String {
    let bytes: u64 = worktrees
        .iter()
        .filter_map(|report| report.usage.as_ref().map(|usage| usage.allocated))
        .sum();
    let candidates = worktrees
        .iter()
        .filter(|report| report.verdict == Verdict::Candidate)
        .count();
    format!(
        "  {:<18}{} linked worktree{}, {}, {} candidate{}\n",
        "worktrees",
        worktrees.len(),
        suffix(worktrees.len()),
        output::bytes(bytes),
        candidates,
        suffix(candidates)
    )
}

fn suffix(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

impl Store {
    fn json(&self) -> Json {
        let (activity, reason, detail, processes) = activity_json(self.activity.as_ref());
        Json::Obj(vec![
            ("store", Json::str(self.store)),
            ("path", Json::path(&self.path)),
            ("verified", Json::Bool(self.verified)),
            ("source_url", Json::str(self.source_url)),
            ("state", Json::str(self.state)),
            (
                "link_target",
                Json::opt(self.link_target.as_deref(), Json::path),
            ),
            ("files", Json::opt(self.files, Json::UInt)),
            ("apparent_bytes", Json::opt(self.apparent, Json::UInt)),
            ("allocated_bytes", Json::opt(self.allocated, Json::UInt)),
            ("last_change", self.last_json()),
            ("activity", activity),
            ("activity_reason", reason),
            ("activity_detail", detail),
            ("processes", processes),
            ("errors", strings(&self.errors)),
        ])
    }

    fn last_json(&self) -> Json {
        match (self.last_unix, self.last_what.as_deref()) {
            (Some(unix), Some(what)) => Json::Obj(vec![
                ("unix", Json::Int(unix)),
                ("at", Json::str(output::rfc3339(unix))),
                ("what", Json::str(what)),
            ]),
            _ => Json::Null,
        }
    }

    fn line(&self, now: i64) -> Option<String> {
        if self.state == "missing" {
            return None;
        }
        let name = format!("  {:<18}", self.store);
        let line = match self.state {
            "ok" | "partial" => {
                let size = self
                    .allocated
                    .map(output::bytes)
                    .unwrap_or_else(|| "-".to_string());
                let files = self
                    .files
                    .map(|n| format!("{} file{}", output::grouped(n), suffix(n as usize)))
                    .unwrap_or_default();
                let newest = self
                    .last_unix
                    .map(|unix| format!("newest {}", output::age(now.saturating_sub(unix))))
                    .unwrap_or_default();
                let activity = activity_word(self.activity.as_ref());
                let partial = if self.state == "partial" {
                    let why = self
                        .errors
                        .first()
                        .map(|error| output::clean(error))
                        .unwrap_or_else(|| "incomplete".to_string());
                    format!("partial: {why}")
                } else {
                    String::new()
                };
                format!("{name}{size:>10}  {files}  {newest}  {activity}  {partial}")
                    .trim_end()
                    .to_string()
            }
            "symlink" => {
                let target = self
                    .link_target
                    .as_ref()
                    .map(|path| output::home(&path.display().to_string()))
                    .unwrap_or_else(|| "an unreadable target".to_string());
                format!("{name}symbolic link to {target}, not measured")
            }
            _ => {
                let why = self
                    .errors
                    .first()
                    .map(|error| output::clean(error))
                    .unwrap_or_else(|| "not read".to_string());
                format!("{name}not read: {why}")
            }
        };
        Some(format!("{line}\n"))
    }
}

fn activity_json(activity: Option<&Evidence>) -> (Json, Json, Json, Json) {
    let Some(activity) = activity else {
        return (Json::Null, Json::Null, Json::Null, Json::Null);
    };
    let processes = match activity {
        Evidence::InUse(procs) => Json::Arr(
            procs
                .iter()
                .map(|(pid, command)| {
                    Json::Obj(vec![
                        ("pid", Json::UInt(u64::from(*pid))),
                        ("command", Json::str(command.clone())),
                    ])
                })
                .collect(),
        ),
        _ => Json::Null,
    };
    let (reason, detail) = match activity {
        Evidence::Unknown(why) => (Json::str(why.reason), Json::str(why.detail.clone())),
        _ => (Json::Null, Json::Null),
    };
    (Json::str(activity.code()), reason, detail, processes)
}

fn activity_word(activity: Option<&Evidence>) -> String {
    match activity {
        Some(Evidence::InUse(_)) => "in use".to_string(),
        Some(Evidence::NotObserved) => "not in use".to_string(),
        Some(Evidence::Unknown(why)) => format!("unknown ({})", why.reason),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::activity::{Seen, Use};
    use std::io::Write;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    use std::time::{Duration, UNIX_EPOCH};

    struct Temp(PathBuf);

    impl Temp {
        fn new() -> Temp {
            let path = std::env::temp_dir().join(format!(
                "residue-agents-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |d| d.as_nanos())
            ));
            fs::create_dir_all(&path).unwrap();
            Temp(path)
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            fn open_up(path: &Path) {
                let Ok(entries) = fs::read_dir(path) else {
                    return;
                };
                for entry in entries.flatten() {
                    let child = entry.path();
                    let meta = fs::symlink_metadata(&child).ok();
                    let _ = fs::set_permissions(&child, fs::Permissions::from_mode(0o755));
                    if meta.is_some_and(|meta| meta.is_dir()) {
                        open_up(&child);
                    }
                }
            }
            open_up(&self.0);
            let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn quiet() -> Snapshot {
        snapshot(Vec::new(), None)
    }

    fn snapshot(uses: Vec<Use>, limits: Option<&'static str>) -> Snapshot {
        Snapshot {
            backend: "test",
            state: Ok(Seen {
                uses,
                inspected: 1,
                total: Some(1),
                limits,
                spelling: None,
                unnamed: Vec::new(),
                hidden: None,
            }),
        }
    }

    fn request<'a>(
        home: &'a Path,
        codex: &'a Path,
        claude: &'a Path,
        roots: &'a [PathBuf],
        snapshot: &'a Snapshot,
    ) -> Request<'a> {
        Request {
            home,
            codex_home: Some(codex),
            claude_config: Some(claude),
            claude_tmpdir: Some(claude),
            tmpdir: None,
            roots,
            snapshot,
        }
    }

    fn store<'a>(report: &'a Report, agent: &str, name: &str) -> &'a Store {
        report
            .agents
            .iter()
            .find(|item| item.agent == agent)
            .unwrap()
            .stores
            .iter()
            .find(|item| item.store == name)
            .unwrap_or_else(|| panic!("{agent} has no {name}"))
    }

    #[test]
    fn sessions_report_size_count_and_newest_file() {
        let tmp = Temp::new();
        let codex = tmp.0.join("codex");
        let day = codex.join("sessions/2026/11/01");
        fs::create_dir_all(&day).unwrap();
        let older = day.join("rollout-old.jsonl");
        let newer = day.join("rollout-new.jsonl");
        fs::write(&older, "old").unwrap();
        fs::write(&newer, "newer-bytes").unwrap();
        let stamp = |path: &Path, secs: u64| {
            fs::File::open(path)
                .unwrap()
                .set_modified(UNIX_EPOCH + Duration::from_secs(secs))
                .unwrap();
        };
        // Directory mtimes move when a file is added, and the walk counts
        // them. Set them behind the newer file so that file is the newest.
        stamp(&codex.join("sessions"), 1_700_000_000);
        stamp(&codex.join("sessions/2026"), 1_700_000_000);
        stamp(&codex.join("sessions/2026/11"), 1_700_000_000);
        stamp(&day, 1_700_000_000);
        stamp(&older, 1_700_000_000);
        stamp(&newer, 1_700_000_100);
        fs::write(codex.join("config.toml"), "secret = true").unwrap();
        fs::write(codex.join("auth.json"), "token").unwrap();
        let claude = tmp.0.join("claude");
        fs::create_dir_all(claude.join("projects")).unwrap();
        fs::write(claude.join("settings.json"), "{}").unwrap();
        fs::write(claude.join(".credentials.json"), "secret").unwrap();
        let snap = quiet();
        let report = build(request(&tmp.0, &codex, &claude, &[], &snap));
        let sessions = store(&report, "codex", "sessions");
        assert_eq!(sessions.state, "ok");
        assert_eq!(sessions.files, Some(2));
        // The walk's apparent size includes the directories that hold the
        // files, so it is at least the two files and not only them.
        assert!(sessions.apparent.unwrap_or(0) >= 3 + 11);
        assert_eq!(sessions.last_unix, Some(1_700_000_100));
        assert_eq!(
            sessions.last_what.as_deref(),
            Some("2026/11/01/rollout-new.jsonl")
        );
        assert!(matches!(sessions.activity, Some(Evidence::NotObserved)));
        let names: Vec<&str> = report
            .agents
            .iter()
            .flat_map(|agent| agent.stores.iter().map(|store| store.store))
            .collect();
        for banned in [
            "config",
            "auth",
            "settings",
            "credentials",
            "plugins",
            "memory",
        ] {
            assert!(
                !names.iter().any(|name| name.contains(banned)),
                "{names:?} names {banned}"
            );
        }
        let rendered = report.json(&snap, 1_700_000_000);
        assert!(rendered.contains("\"schema\":\"residue.agents/v1\""));
        assert!(!rendered.contains("config.toml"));
        assert!(!rendered.contains("auth.json"));
        assert!(!rendered.contains("settings.json"));
        assert!(!rendered.contains(".credentials.json"));
    }

    #[test]
    fn missing_unreadable_and_symlink_stores_stay_unmeasured() {
        let tmp = Temp::new();
        let codex = tmp.0.join("codex");
        fs::create_dir_all(&codex).unwrap();
        let real = tmp.0.join("real-sessions");
        fs::create_dir_all(&real).unwrap();
        fs::write(real.join("rollout.jsonl"), "x".repeat(50_000)).unwrap();
        std::os::unix::fs::symlink(&real, codex.join("sessions")).unwrap();
        let outside = tmp.0.join("outside.bin");
        fs::write(&outside, vec![1u8; 200_000]).unwrap();
        let logs = codex.join("log");
        fs::create_dir_all(logs.join("kept")).unwrap();
        fs::write(logs.join("kept").join("a.log"), "log").unwrap();
        std::os::unix::fs::symlink(&outside, logs.join("leap")).unwrap();
        let blocked = logs.join("blocked");
        fs::create_dir_all(&blocked).unwrap();
        fs::write(blocked.join("secret"), "nope").unwrap();
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o0)).unwrap();
        let claude = tmp.0.join("absent-claude");
        let snap = quiet();
        let report = build(request(&tmp.0, &codex, &claude, &[], &snap));
        let sessions = store(&report, "codex", "sessions");
        assert_eq!(sessions.state, "symlink");
        assert_eq!(sessions.apparent, None);
        assert_eq!(sessions.link_target.as_deref(), Some(real.as_path()));
        let history = store(&report, "codex", "history");
        assert_eq!(history.state, "missing");
        assert_eq!(history.files, None);
        let archived = store(&report, "codex", "archived_sessions");
        assert_eq!(archived.state, "missing");
        let logs = store(&report, "codex", "logs");
        // SAFETY: geteuid only returns this process's effective user.
        let root = unsafe { libc::geteuid() } == 0;
        if root {
            assert_eq!(logs.state, "ok", "root can read a mode 000 directory");
        } else {
            assert_eq!(logs.state, "partial");
            assert!(
                logs.errors.iter().any(|error| error.contains("blocked")),
                "{:?}",
                logs.errors
            );
        }
        assert!(logs.apparent.unwrap_or(0) < 200_000);
        assert!(store(&report, "claude", "projects").state == "missing");
    }

    #[test]
    fn a_write_only_rollout_is_sized_without_being_opened() {
        let tmp = Temp::new();
        let codex = tmp.0.join("codex");
        let day = codex.join("sessions");
        fs::create_dir_all(&day).unwrap();
        let rollout = day.join("rollout-x.jsonl");
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o200)
            .open(&rollout)
            .unwrap();
        file.write_all(b"transcript").unwrap();
        drop(file);
        let claude = tmp.0.join("claude");
        fs::create_dir_all(&claude).unwrap();
        let snap = quiet();
        let report = build(request(&tmp.0, &codex, &claude, &[], &snap));
        let sessions = store(&report, "codex", "sessions");
        // SAFETY: geteuid only returns this process's effective user.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        assert_eq!(sessions.state, "ok");
        assert!(sessions.apparent.unwrap_or(0) >= "transcript".len() as u64);
    }

    #[test]
    fn an_open_rollout_marks_the_store_in_use_and_a_limited_probe_unknown() {
        let tmp = Temp::new();
        let codex = tmp.0.join("codex");
        let day = codex.join("sessions");
        fs::create_dir_all(&day).unwrap();
        let written = day.join("rollout-x.jsonl");
        fs::write(&written, "hello").unwrap();
        let rollout = fs::canonicalize(&written).unwrap();
        let claude = tmp.0.join("claude");
        fs::create_dir_all(&claude).unwrap();
        let snap = snapshot(
            vec![Use {
                pid: 42,
                command: "sleep".into(),
                path: rollout.as_os_str().as_bytes().to_vec(),
            }],
            None,
        );
        let report = build(request(&tmp.0, &codex, &claude, &[], &snap));
        let sessions = store(&report, "codex", "sessions");
        assert!(
            matches!(&sessions.activity, Some(Evidence::InUse(procs)) if procs.iter().any(|(pid, _)| *pid == 42)),
            "sessions was not in use by pid 42"
        );
        let limited = snapshot(Vec::new(), Some("working directories, not open files"));
        let report = build(request(&tmp.0, &codex, &claude, &[], &limited));
        let sessions = store(&report, "codex", "sessions");
        assert!(
            matches!(&sessions.activity, Some(Evidence::Unknown(why)) if why.reason == "partial_probe"),
            "a limited probe was not partial_probe"
        );
    }

    #[test]
    fn sqlite_files_are_counted_and_a_link_is_not_followed() {
        let tmp = Temp::new();
        let codex = tmp.0.join("codex");
        fs::create_dir_all(&codex).unwrap();
        fs::write(codex.join("state_5.sqlite"), "db").unwrap();
        fs::write(codex.join("logs_2.sqlite"), "logs").unwrap();
        let outside = tmp.0.join("big.sqlite");
        fs::write(&outside, vec![7u8; 80_000]).unwrap();
        std::os::unix::fs::symlink(&outside, codex.join("moved.sqlite")).unwrap();
        fs::write(codex.join("notes.txt"), "not a database").unwrap();
        let claude = tmp.0.join("claude");
        fs::create_dir_all(&claude).unwrap();
        let snap = quiet();
        let report = build(request(&tmp.0, &codex, &claude, &[], &snap));
        let db = store(&report, "codex", "state_db");
        assert_eq!(db.state, "partial");
        assert_eq!(db.files, Some(2));
        assert_eq!(db.apparent, Some(2 + 4));
        assert!(db.errors.iter().any(|error| error.contains("moved.sqlite")));
    }

    #[test]
    fn worktree_owner_follows_the_directory_not_a_prefix() {
        let codex = Path::new("/home/me/.codex/worktrees");
        assert_eq!(
            owner(Path::new("/home/me/.codex/worktrees/app"), Some(codex)),
            Some("codex")
        );
        assert_eq!(
            owner(Path::new("/home/me/.codex/worktrees-old/app"), Some(codex)),
            None
        );
        assert_eq!(
            owner(Path::new("/repo/.claude/worktrees/fix"), None),
            Some("claude")
        );
        assert_eq!(owner(Path::new("/repo/.claude/skills/x"), None), None);
    }

    #[test]
    fn an_agent_with_nothing_on_disk_is_in_the_json_and_not_the_text() {
        let tmp = Temp::new();
        let codex = tmp.0.join("no-codex");
        let claude = tmp.0.join("no-claude");
        let snap = quiet();
        let report = build(request(&tmp.0, &codex, &claude, &[], &snap));
        assert!(
            report
                .agents
                .iter()
                .all(|agent| { agent.stores.iter().all(|store| store.state == "missing") })
        );
        let text = report.text(&snap, 1_700_000_000);
        assert!(text.contains("no Codex or Claude Code store found"));
        assert!(!text.contains("codex ·"));
        let json = report.json(&snap, 1_700_000_000);
        assert!(json.contains("\"agent\":\"codex\""));
        assert!(json.contains("\"agent\":\"claude\""));
        assert!(json.contains("\"state\":\"missing\""));
    }
}
