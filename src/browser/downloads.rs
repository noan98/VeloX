//! Download management: state machine, filename sanitization, same-name
//! collision avoidance, download-directory resolution, and the (program,
//! args) pairs used to open a completed file / the downloads folder.
//!
//! Everything in this module is plain data, pure functions, or a thin,
//! deliberately "dumb" IO wrapper around them (mirroring
//! `browser::persistence`'s split — see its module doc comment and
//! docs/decisions.md D10) — no `wry`/`tao`/`gtk` dependency anywhere here,
//! per the same `browser::` boundary every other module in this directory
//! keeps (see docs/decisions.md D20). `ui::window` is the only place that
//! talks to wry's actual download callbacks; it calls straight into the
//! functions below to decide a destination and never re-implements any of
//! this logic itself.
//!
//! See docs/decisions.md D28 for why VeloX does not (and, per wry 0.56's
//! public API, cannot) offer byte-level progress or true mid-transfer
//! cancellation.

use std::path::{Path, PathBuf};
use std::process::{Child, Command};

/// Opaque, stable identifier for one download, issued by [`DownloadStore`]
/// and never reused — same shape and reasoning as `browser::TabId` (a stale
/// id, e.g. a `cancel_download` message racing a completion, simply misses
/// cleanly instead of acting on the wrong entry).
///
/// Serializes as a bare JSON number (`#[serde(transparent)]`), matching how
/// `ui::toolbar::TabSummary` exposes tab ids to the toolbar's JS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize)]
#[serde(transparent)]
pub struct DownloadId(u64);

impl DownloadId {
    /// The underlying numeric id, e.g. to embed in a toolbar IPC message.
    pub fn get(self) -> u64 {
        self.0
    }
}

impl From<u64> for DownloadId {
    fn from(id: u64) -> Self {
        DownloadId(id)
    }
}

/// A download's lifecycle state.
///
/// There is no separate "requested, not yet decided" state before
/// [`DownloadState::InProgress`]: unlike `browser::TabState`'s `Restoring`
/// (added ahead of a concrete future need — see docs/decisions.md D20),
/// there is no wry callback that could ever observe such a gap — wry's
/// `download_started_handler` decides accept/reject and returns
/// synchronously in one call (see docs/decisions.md D28), so by the time
/// VeloX has anything to show the user, the download is already either
/// happening or was never created. [`DownloadStore::start`] therefore
/// creates every entry directly as `InProgress`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DownloadState {
    /// Actively being written by the engine. wry exposes no byte-count
    /// callback (see D28), so this is the only "in flight" state VeloX can
    /// observe — there is no percentage to attach to it.
    InProgress,
    /// Finished successfully; `DownloadEntry::destination` is the final
    /// file.
    Completed,
    /// Finished unsuccessfully; `DownloadEntry::error` holds a
    /// human-readable reason when one is available.
    Failed,
    /// The user asked to cancel it. See docs/decisions.md D28: this is a
    /// best-effort local bookkeeping/cleanup action, not a true abort of an
    /// in-flight transfer — wry 0.56 exposes no handle to stop one.
    Cancelled,
}

impl DownloadState {
    /// Whether this is one of the three terminal states — no further
    /// transition is ever valid from here.
    pub fn is_terminal(self) -> bool {
        !matches!(self, DownloadState::InProgress)
    }

    fn complete(self) -> Result<Self, InvalidDownloadTransition> {
        match self {
            DownloadState::InProgress => Ok(DownloadState::Completed),
            _ => Err(InvalidDownloadTransition {
                from: self,
                to: DownloadState::Completed,
            }),
        }
    }

    fn fail(self) -> Result<Self, InvalidDownloadTransition> {
        match self {
            DownloadState::InProgress => Ok(DownloadState::Failed),
            _ => Err(InvalidDownloadTransition {
                from: self,
                to: DownloadState::Failed,
            }),
        }
    }

    fn cancel(self) -> Result<Self, InvalidDownloadTransition> {
        match self {
            DownloadState::InProgress => Ok(DownloadState::Cancelled),
            _ => Err(InvalidDownloadTransition {
                from: self,
                to: DownloadState::Cancelled,
            }),
        }
    }
}

/// A state transition that is not one of [`DownloadState`]'s defined edges
/// was attempted (mirrors `browser::tab::InvalidTabTransition`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidDownloadTransition {
    pub from: DownloadState,
    pub to: DownloadState,
}

impl std::fmt::Display for InvalidDownloadTransition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "invalid download state transition: {:?} -> {:?}",
            self.from, self.to
        )
    }
}

impl std::error::Error for InvalidDownloadTransition {}

/// One tracked download.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct DownloadEntry {
    pub id: DownloadId,
    pub url: String,
    /// The sanitized, collision-avoided file name (no directory component) —
    /// see [`sanitize_filename`] / [`unique_filename`].
    pub file_name: String,
    /// The full path the file is (or was) being written to.
    #[serde(serialize_with = "serialize_path_lossy")]
    pub destination: PathBuf,
    pub state: DownloadState,
    /// Unix timestamp (seconds) the download started.
    pub started_at: u64,
    /// Unix timestamp (seconds) the download reached a terminal state.
    /// `None` while `state` is `InProgress`.
    pub finished_at: Option<u64>,
    /// A human-readable failure reason, set only by [`DownloadStore::fail`].
    pub error: Option<String>,
}

fn serialize_path_lossy<S: serde::Serializer>(path: &Path, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&path.to_string_lossy())
}

impl DownloadEntry {
    fn complete(&mut self, finished_at: u64) -> Result<(), InvalidDownloadTransition> {
        self.state = self.state.complete()?;
        self.finished_at = Some(finished_at);
        Ok(())
    }

    fn fail(&mut self, reason: String, finished_at: u64) -> Result<(), InvalidDownloadTransition> {
        self.state = self.state.fail()?;
        self.finished_at = Some(finished_at);
        self.error = Some(reason);
        Ok(())
    }

    fn cancel(&mut self, finished_at: u64) -> Result<(), InvalidDownloadTransition> {
        self.state = self.state.cancel()?;
        self.finished_at = Some(finished_at);
        Ok(())
    }
}

/// An in-memory collection of [`DownloadEntry`] values, in the order they
/// started.
///
/// Deliberately not persisted to disk (unlike `HistoryStore`/`BookmarkStore`
/// via `browser::persistence`): the issue's acceptance criteria only call
/// for a session-scoped download list, `browser::persistence` is Issue #18's
/// file to change (not this one's), and a separate persistence file was not
/// worth adding speculatively — see docs/decisions.md D28.
#[derive(Debug, Clone, Default)]
pub struct DownloadStore {
    entries: Vec<DownloadEntry>,
    next_id: u64,
}

impl DownloadStore {
    /// A new, empty store.
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            next_id: 1,
        }
    }

    /// All entries, oldest first.
    pub fn entries(&self) -> &[DownloadEntry] {
        &self.entries
    }

    /// Entries, most recently started first — the order the UI panel lists
    /// them in (mirrors `HistoryStore::entries_newest_first`).
    pub fn entries_newest_first(&self) -> impl Iterator<Item = &DownloadEntry> {
        self.entries.iter().rev()
    }

    pub fn get(&self, id: DownloadId) -> Option<&DownloadEntry> {
        self.entries.iter().find(|entry| entry.id == id)
    }

    fn get_mut(&mut self, id: DownloadId) -> Option<&mut DownloadEntry> {
        self.entries.iter_mut().find(|entry| entry.id == id)
    }

    /// Register a download that wry's `download_started_handler` has just
    /// accepted, as `InProgress`. `destination` should already be the final,
    /// sanitized, collision-avoided path (see [`build_destination`] /
    /// [`prepare_destination`]) — this store does not sanitize anything
    /// itself, matching how `HistoryStore`/`BookmarkStore` do not validate
    /// the URLs they are given.
    pub fn start(
        &mut self,
        url: String,
        file_name: String,
        destination: PathBuf,
        started_at: u64,
    ) -> DownloadId {
        let id = DownloadId(self.next_id);
        self.next_id += 1;
        self.entries.push(DownloadEntry {
            id,
            url,
            file_name,
            destination,
            state: DownloadState::InProgress,
            started_at,
            finished_at: None,
            error: None,
        });
        id
    }

    /// `InProgress -> Completed`. Returns `false` (no-op) for an unknown id
    /// or an entry that is not currently `InProgress`.
    pub fn complete(&mut self, id: DownloadId, finished_at: u64) -> bool {
        self.get_mut(id)
            .is_some_and(|entry| entry.complete(finished_at).is_ok())
    }

    /// `InProgress -> Failed`, recording `reason`. Returns `false` (no-op)
    /// the same way [`Self::complete`] does.
    pub fn fail(&mut self, id: DownloadId, reason: String, finished_at: u64) -> bool {
        self.get_mut(id)
            .is_some_and(|entry| entry.fail(reason, finished_at).is_ok())
    }

    /// `InProgress -> Cancelled`. Returns `false` (no-op) the same way
    /// [`Self::complete`] does — including, deliberately, when the entry has
    /// already reached a terminal state (a late completion racing a cancel
    /// request must not be un-cancelled).
    pub fn cancel(&mut self, id: DownloadId, finished_at: u64) -> bool {
        self.get_mut(id)
            .is_some_and(|entry| entry.cancel(finished_at).is_ok())
    }

    /// Drop one entry from the list entirely (the panel's "remove" button —
    /// never touches the file on disk). Returns `true` if an entry with
    /// this id existed.
    pub fn remove(&mut self, id: DownloadId) -> bool {
        let before = self.entries.len();
        self.entries.retain(|entry| entry.id != id);
        self.entries.len() != before
    }

    /// Find the id of the `InProgress` entry that wry's
    /// `download_completed_handler(url, path, success)` most likely refers
    /// to.
    ///
    /// wry's completed callback hands back no id of its own — only the
    /// original URL and, on some platforms, the resolved destination path
    /// (always `None` on macOS; see docs/decisions.md D28) — so this is a
    /// best-effort match: an exact destination match wins when `destination`
    /// is `Some` and matches an in-progress entry, otherwise the oldest
    /// still-`InProgress` entry for `url` is assumed (correct for the
    /// overwhelmingly common case of at most one active download per URL;
    /// ambiguous only if the same URL is downloaded twice concurrently,
    /// documented as a known limitation).
    pub fn resolve_completion(&self, url: &str, destination: Option<&Path>) -> Option<DownloadId> {
        if let Some(destination) = destination {
            if let Some(entry) = self.entries.iter().find(|entry| {
                entry.state == DownloadState::InProgress
                    && entry.url == url
                    && entry.destination == destination
            }) {
                return Some(entry.id);
            }
        }
        self.entries
            .iter()
            .find(|entry| entry.state == DownloadState::InProgress && entry.url == url)
            .map(|entry| entry.id)
    }
}

// --- Filename sanitization (security-critical: see docs/decisions.md D28
// and the issue's own "ファイル名のサニタイズ" note) ---

/// Fallback name used when sanitizing `raw` leaves nothing usable (empty,
/// `.`/`..`, or entirely control characters).
const FALLBACK_FILENAME: &str = "download";

/// Safe ceiling on a sanitized file name's length in bytes, comfortably
/// under common filesystem component limits (255 bytes on ext4/NTFS/APFS),
/// leaving headroom for a `unique_filename` collision suffix like
/// `" (1234)"`.
const MAX_FILENAME_BYTES: usize = 200;

/// Turn a server-supplied (untrusted) suggested file name into a safe, bare
/// file name with no directory component.
///
/// Handles, in order:
/// - **Path traversal / absolute paths / embedded separators**: only the
///   last `/`- or `\`-separated segment is kept, so `../../etc/passwd`,
///   `/etc/passwd`, and `C:\Windows\System32\evil.exe` all collapse to a
///   bare `passwd`/`evil.exe` with no directory component at all — there is
///   nothing left for a resulting path to "traverse" with.
/// - **NUL and other control characters**: stripped outright.
/// - **`.` / `..` as the whole name**: rejected (falls through to the
///   fallback name below), since neither is a usable file name once no
///   directory component exists to traverse into.
/// - **Trailing dots/spaces**: trimmed (Windows does not allow either at the
///   end of a file name).
/// - **Windows reserved device names** (`CON`, `PRN`, `AUX`, `NUL`,
///   `COM1`-`COM9`, `LPT1`-`LPT9`, matched case-insensitively against the
///   name up to its first `.`): prefixed with `_` so `CON.txt` becomes
///   `_CON.txt` rather than colliding with a reserved device name on
///   Windows.
/// - **Excessive length**: truncated to [`MAX_FILENAME_BYTES`], preserving
///   the extension when there is a reasonably-sized one.
/// - **Nothing usable left**: falls back to [`FALLBACK_FILENAME`].
pub fn sanitize_filename(raw: &str) -> String {
    let basename = raw.rsplit(['/', '\\']).next().unwrap_or("");
    let cleaned: String = basename.chars().filter(|c| !c.is_control()).collect();
    let trimmed = cleaned.trim();

    let mut name = if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        String::new()
    } else {
        trimmed.to_owned()
    };

    while name.ends_with('.') || name.ends_with(' ') {
        name.pop();
    }

    if name.is_empty() {
        return FALLBACK_FILENAME.to_owned();
    }

    if is_windows_reserved_name(&name) {
        name = format!("_{name}");
    }

    name = truncate_filename(&name, MAX_FILENAME_BYTES);

    if name.is_empty() {
        FALLBACK_FILENAME.to_owned()
    } else {
        name
    }
}

fn is_windows_reserved_name(name: &str) -> bool {
    let base = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
    if matches!(base.as_str(), "CON" | "PRN" | "AUX" | "NUL") {
        return true;
    }
    if let Some(rest) = base
        .strip_prefix("COM")
        .or_else(|| base.strip_prefix("LPT"))
    {
        return rest.len() == 1 && rest.starts_with(|c: char| c.is_ascii_digit() && c != '0');
    }
    false
}

/// Split `filename` into `(stem, extension)`, taking the extension to be
/// everything after the *first* `.` — so `archive.tar.gz` splits into
/// `("archive", Some("tar.gz"))`, keeping compound extensions intact for
/// [`unique_filename`]'s collision suffix (`archive (1).tar.gz`, not
/// `archive.tar (1).gz`). This is the same split wry's own bundled
/// WKWebView download-destination logic uses internally (see
/// docs/decisions.md D28), reused here for consistency rather than
/// re-deriving a different convention.
///
/// A leading-dot name with no other `.` (`.gitignore`) or a name with no `.`
/// at all (`README`) has no stem to split off, so the whole name is kept as
/// the stem with no extension.
fn split_stem_and_ext(filename: &str) -> (String, Option<String>) {
    match filename.split_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem.to_owned(), Some(ext.to_owned())),
        _ => (filename.to_owned(), None),
    }
}

fn truncate_filename(name: &str, max_bytes: usize) -> String {
    if name.len() <= max_bytes {
        return name.to_owned();
    }
    let (stem, ext) = split_stem_and_ext(name);
    match ext {
        Some(ext) if ext.len() + 1 < max_bytes => {
            let budget = max_bytes - ext.len() - 1;
            format!("{}.{ext}", truncate_at_char_boundary(&stem, budget))
        }
        _ => truncate_at_char_boundary(name, max_bytes),
    }
}

fn truncate_at_char_boundary(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_owned();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_owned()
}

/// Given a predicate telling whether a candidate name is already taken,
/// return `filename` unchanged if it is free, otherwise the first
/// `"{stem} ({n}){ext}"` variant (`n` starting at 1) that is not — the
/// `report.pdf` / `report (1).pdf` scheme the issue asks for.
///
/// `exists` is injected (rather than this function touching the filesystem
/// itself) so the collision logic is unit-testable without a temp
/// directory; [`build_destination`] is the real-filesystem wrapper around
/// this.
pub fn unique_filename(filename: &str, exists: impl Fn(&str) -> bool) -> String {
    if !exists(filename) {
        return filename.to_owned();
    }
    let (stem, ext) = split_stem_and_ext(filename);
    // Bounded rather than an unconditional `loop`: a real filesystem's
    // `exists` predicate is guaranteed to eventually return `false` (it
    // cannot hold infinitely many distinct files), but bounding this
    // defends against a pathological/malicious `exists` closure (e.g. in a
    // test) looping forever. Past the bound, a timestamp-based suffix
    // guarantees termination without ever repeating a previous guess.
    const MAX_ATTEMPTS: u32 = 10_000;
    for counter in 1..=MAX_ATTEMPTS {
        let candidate = match &ext {
            Some(ext) => format!("{stem} ({counter}).{ext}"),
            None => format!("{stem} ({counter})"),
        };
        if !exists(&candidate) {
            return candidate;
        }
    }
    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    match &ext {
        Some(ext) => format!("{stem} ({suffix}).{ext}"),
        None => format!("{stem} ({suffix})"),
    }
}

/// Sanitize `raw_filename` and resolve a collision-free absolute path inside
/// `dir` by checking the real filesystem (`Path::exists`). Does not create
/// `dir` itself — see [`prepare_destination`] for the IO-performing
/// wrapper `ui::window` actually calls.
pub fn build_destination(dir: &Path, raw_filename: &str) -> PathBuf {
    let sanitized = sanitize_filename(raw_filename);
    let chosen = unique_filename(&sanitized, |candidate| dir.join(candidate).exists());
    dir.join(chosen)
}

/// [`build_destination`], but first creates `dir` (and any missing parents)
/// if it does not exist yet — the download destination directory must exist
/// before the engine can start writing to it. Mirrors
/// `persistence::write_json`'s "create the directory, then act" shape (see
/// its module doc comment).
pub fn prepare_destination(dir: &Path, raw_filename: &str) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    Ok(build_destination(dir, raw_filename))
}

// --- Download directory resolution (mirrors `persistence`'s env-var
// pattern — see docs/decisions.md D10 — without touching that file, which
// Issue #18 owns) ---

/// Resolve the directory VeloX saves downloads into.
///
/// `VELOX_DOWNLOAD_DIR`, when set, always wins (used by tests, and lets a
/// user redirect downloads without a settings UI). Otherwise this follows
/// each platform's usual convention, resolved from environment variables
/// that are already present rather than a `dirs`-style crate (same
/// reasoning as `persistence::default_data_dir`, D10): `XDG_DOWNLOAD_DIR` or
/// `$HOME/Downloads` on Linux/BSD, `$HOME/Downloads` on macOS,
/// `%USERPROFILE%\Downloads` on Windows. Returns `None` when no suitable
/// environment variable is set.
pub fn resolve_download_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("VELOX_DOWNLOAD_DIR") {
        return Some(PathBuf::from(dir));
    }
    platform_download_dir()
}

#[cfg(target_os = "macos")]
fn platform_download_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Downloads"))
}

#[cfg(target_os = "windows")]
fn platform_download_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE").map(|dir| PathBuf::from(dir).join("Downloads"))
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn platform_download_dir() -> Option<PathBuf> {
    let xdg = std::env::var("XDG_DOWNLOAD_DIR").ok();
    let home = std::env::var("HOME").ok();
    resolve_unix_download_dir(xdg.as_deref(), home.as_deref())
}

/// Pure decision logic behind the non-macOS/Windows branch of
/// [`platform_download_dir`], taking already-read environment values
/// instead of touching the real process environment — the same
/// testability split `config::resolve_private`/`resolve_perf_env` use for
/// `Config::from_env_and_args`. Exercised directly by unit tests; CI for
/// this project only runs on Linux (see CLAUDE.md), so this is the one
/// platform branch with real test coverage — matching the existing
/// Linux-only coverage gap `persistence::platform_data_dir` already has for
/// its own macOS/Windows branches.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn resolve_unix_download_dir(
    xdg_download_dir: Option<&str>,
    home: Option<&str>,
) -> Option<PathBuf> {
    if let Some(xdg) = xdg_download_dir {
        return Some(PathBuf::from(xdg));
    }
    home.map(|home| PathBuf::from(home).join("Downloads"))
}

// --- Opening a completed file / the downloads folder ---
//
// "完了ファイルを開く" / "ダウンロードフォルダを開く": both are the same OS
// primitive — hand a path to the platform's default-application launcher —
// so one function covers both call sites (`ui::toolbar`'s "open" button
// passes a file's `destination`; its "open downloads folder" button passes
// `resolve_download_dir()`'s result directly). Building `(program, args)`
// is kept separate from actually spawning the process so the command
// construction itself is unit-testable without a display or a real
// external application — see [`spawn_open`] for the IO-performing half.

/// The `(program, args)` pair that opens `path` with the OS's default
/// handler, to run via `Command::new(program).args(args)` — **never**
/// through a shell (`sh -c`), since `path` can contain characters from a
/// server-supplied file name; passing it as a single argument to the OS
/// launcher directly (not interpolated into a shell command string) means
/// there is no shell metacharacter for it to be misinterpreted as.
#[cfg(target_os = "macos")]
pub fn open_path_command(path: &Path) -> (&'static str, Vec<String>) {
    ("open", vec![path.to_string_lossy().into_owned()])
}

#[cfg(target_os = "windows")]
pub fn open_path_command(path: &Path) -> (&'static str, Vec<String>) {
    ("explorer", vec![path.to_string_lossy().into_owned()])
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn open_path_command(path: &Path) -> (&'static str, Vec<String>) {
    ("xdg-open", vec![path.to_string_lossy().into_owned()])
}

/// Spawn [`open_path_command`] for `path` and detach — VeloX never waits on
/// or otherwise tracks the launched process, the same "fire and forget" a
/// real desktop browser's "show in folder" / "open file" action is. Not
/// exercised by any test: this project's CI/dev environment is headless, so
/// there is no `xdg-open`-reachable desktop session to actually launch
/// something in (see docs/decisions.md D28's "what's unverified" note); the
/// command-construction half above is what carries this file's test
/// coverage for the launch path.
pub fn spawn_open(path: &Path) -> std::io::Result<Child> {
    let (program, args) = open_path_command(path);
    Command::new(program).args(args).spawn()
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- DownloadState / DownloadEntry transitions ---

    #[test]
    fn store_start_creates_an_in_progress_entry() {
        let mut store = DownloadStore::new();
        let id = store.start(
            "https://example.com/report.pdf".to_owned(),
            "report.pdf".to_owned(),
            PathBuf::from("/tmp/downloads/report.pdf"),
            100,
        );
        let entry = store.get(id).unwrap();
        assert_eq!(entry.state, DownloadState::InProgress);
        assert_eq!(entry.started_at, 100);
        assert_eq!(entry.finished_at, None);
        assert_eq!(entry.error, None);
    }

    #[test]
    fn complete_transitions_from_in_progress_and_sets_finished_at() {
        let mut store = DownloadStore::new();
        let id = store.start("u".into(), "f".into(), PathBuf::from("/tmp/f"), 1);
        assert!(store.complete(id, 5));
        let entry = store.get(id).unwrap();
        assert_eq!(entry.state, DownloadState::Completed);
        assert_eq!(entry.finished_at, Some(5));
    }

    #[test]
    fn fail_transitions_from_in_progress_and_records_reason() {
        let mut store = DownloadStore::new();
        let id = store.start("u".into(), "f".into(), PathBuf::from("/tmp/f"), 1);
        assert!(store.fail(id, "network error".to_owned(), 9));
        let entry = store.get(id).unwrap();
        assert_eq!(entry.state, DownloadState::Failed);
        assert_eq!(entry.error.as_deref(), Some("network error"));
        assert_eq!(entry.finished_at, Some(9));
    }

    #[test]
    fn cancel_transitions_from_in_progress() {
        let mut store = DownloadStore::new();
        let id = store.start("u".into(), "f".into(), PathBuf::from("/tmp/f"), 1);
        assert!(store.cancel(id, 3));
        assert_eq!(store.get(id).unwrap().state, DownloadState::Cancelled);
    }

    #[test]
    fn terminal_states_reject_every_further_transition() {
        let mut store = DownloadStore::new();
        let id = store.start("u".into(), "f".into(), PathBuf::from("/tmp/f"), 1);
        assert!(store.complete(id, 2));
        // Already Completed: every further transition is a no-op, not a
        // state change — this is exactly the "a late completion must not
        // resurrect a cancelled entry" guarantee.
        assert!(!store.complete(id, 3));
        assert!(!store.fail(id, "x".to_owned(), 3));
        assert!(!store.cancel(id, 3));
        assert_eq!(store.get(id).unwrap().state, DownloadState::Completed);
        assert_eq!(store.get(id).unwrap().finished_at, Some(2));
    }

    #[test]
    fn unknown_id_transitions_are_a_noop_not_a_panic() {
        let mut store = DownloadStore::new();
        let bogus = DownloadId::from(999);
        assert!(!store.complete(bogus, 1));
        assert!(!store.fail(bogus, "x".to_owned(), 1));
        assert!(!store.cancel(bogus, 1));
        assert!(store.get(bogus).is_none());
    }

    #[test]
    fn is_terminal_matches_the_three_terminal_states() {
        assert!(!DownloadState::InProgress.is_terminal());
        assert!(DownloadState::Completed.is_terminal());
        assert!(DownloadState::Failed.is_terminal());
        assert!(DownloadState::Cancelled.is_terminal());
    }

    #[test]
    fn remove_drops_an_entry_and_ids_stay_unique_afterwards() {
        let mut store = DownloadStore::new();
        let first = store.start("u".into(), "f".into(), PathBuf::from("/tmp/f"), 1);
        assert!(store.remove(first));
        assert!(store.get(first).is_none());
        assert!(!store.remove(first));
        let second = store.start("u".into(), "f".into(), PathBuf::from("/tmp/f"), 2);
        assert_ne!(first, second);
    }

    #[test]
    fn entries_newest_first_reverses_start_order() {
        let mut store = DownloadStore::new();
        store.start("a".into(), "a".into(), PathBuf::from("/tmp/a"), 1);
        store.start("b".into(), "b".into(), PathBuf::from("/tmp/b"), 2);
        let urls: Vec<&str> = store
            .entries_newest_first()
            .map(|e| e.url.as_str())
            .collect();
        assert_eq!(urls, ["b", "a"]);
    }

    // --- resolve_completion correlation ---

    #[test]
    fn resolve_completion_matches_by_url_when_only_one_is_in_progress() {
        let mut store = DownloadStore::new();
        let id = store.start(
            "https://example.com/f".to_owned(),
            "f".into(),
            PathBuf::from("/tmp/f"),
            1,
        );
        assert_eq!(
            store.resolve_completion("https://example.com/f", None),
            Some(id)
        );
    }

    #[test]
    fn resolve_completion_prefers_exact_destination_match() {
        let mut store = DownloadStore::new();
        let first = store.start(
            "https://example.com/f".to_owned(),
            "f".into(),
            PathBuf::from("/tmp/f"),
            1,
        );
        let second = store.start(
            "https://example.com/f".to_owned(),
            "f (1)".into(),
            PathBuf::from("/tmp/f (1)"),
            2,
        );
        assert_eq!(
            store.resolve_completion("https://example.com/f", Some(Path::new("/tmp/f (1)"))),
            Some(second)
        );
        assert_eq!(
            store.resolve_completion("https://example.com/f", Some(Path::new("/tmp/f"))),
            Some(first)
        );
    }

    #[test]
    fn resolve_completion_falls_back_to_oldest_in_progress_for_the_url_without_a_path() {
        let mut store = DownloadStore::new();
        let first = store.start(
            "https://example.com/f".to_owned(),
            "f".into(),
            PathBuf::from("/tmp/f"),
            1,
        );
        store.start(
            "https://example.com/f".to_owned(),
            "f (1)".into(),
            PathBuf::from("/tmp/f (1)"),
            2,
        );
        // macOS never hands back a path (see docs/decisions.md D28) — the
        // FIFO fallback is what runs there.
        assert_eq!(
            store.resolve_completion("https://example.com/f", None),
            Some(first)
        );
    }

    #[test]
    fn resolve_completion_ignores_completed_entries() {
        let mut store = DownloadStore::new();
        let id = store.start("u".into(), "f".into(), PathBuf::from("/tmp/f"), 1);
        store.complete(id, 2);
        assert_eq!(store.resolve_completion("u", None), None);
    }

    #[test]
    fn resolve_completion_returns_none_for_an_unrelated_url() {
        let mut store = DownloadStore::new();
        store.start("u".into(), "f".into(), PathBuf::from("/tmp/f"), 1);
        assert_eq!(store.resolve_completion("other", None), None);
    }

    // --- sanitize_filename: security-critical, tested heavily ---

    #[test]
    fn keeps_an_ordinary_filename_unchanged() {
        assert_eq!(sanitize_filename("report.pdf"), "report.pdf");
        assert_eq!(sanitize_filename("archive.tar.gz"), "archive.tar.gz");
    }

    #[test]
    fn keeps_unicode_filenames_unchanged() {
        assert_eq!(sanitize_filename("レポート.pdf"), "レポート.pdf");
    }

    #[test]
    fn strips_relative_path_traversal_to_the_basename() {
        assert_eq!(sanitize_filename("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_filename("..\\..\\cmd.exe"), "cmd.exe");
        assert_eq!(sanitize_filename("a/b/../../c.txt"), "c.txt");
    }

    #[test]
    fn strips_absolute_paths_to_the_basename() {
        assert_eq!(sanitize_filename("/etc/passwd"), "passwd");
        assert_eq!(
            sanitize_filename(r"C:\Windows\System32\evil.exe"),
            "evil.exe"
        );
    }

    #[test]
    fn rejects_bare_dot_and_dotdot() {
        assert_eq!(sanitize_filename("."), FALLBACK_FILENAME);
        assert_eq!(sanitize_filename(".."), FALLBACK_FILENAME);
        // Also once basename-extracted: a traversal segment as the whole
        // suggested name, not just a component of it.
        assert_eq!(sanitize_filename("../.."), FALLBACK_FILENAME);
    }

    #[test]
    fn strips_nul_and_other_control_characters() {
        assert_eq!(sanitize_filename("evil\0.txt"), "evil.txt");
        assert_eq!(sanitize_filename("a\nb\tc.txt"), "abc.txt");
    }

    #[test]
    fn trims_trailing_dots_and_spaces() {
        assert_eq!(sanitize_filename("evil.txt..."), "evil.txt");
        assert_eq!(sanitize_filename("evil.txt   "), "evil.txt");
        // Trailing dots and spaces are stripped regardless of how they are
        // interleaved, not just a single trailing run of one kind.
        assert_eq!(sanitize_filename("evil.txt. . "), "evil.txt");
    }

    #[test]
    fn empty_or_whitespace_only_falls_back() {
        assert_eq!(sanitize_filename(""), FALLBACK_FILENAME);
        assert_eq!(sanitize_filename("   "), FALLBACK_FILENAME);
        assert_eq!(sanitize_filename("...."), FALLBACK_FILENAME);
        assert_eq!(sanitize_filename("\0\0\0"), FALLBACK_FILENAME);
    }

    #[test]
    fn escapes_windows_reserved_device_names_case_insensitively() {
        for name in ["CON", "con", "PRN", "AUX", "NUL", "Nul"] {
            let sanitized = sanitize_filename(name);
            assert_eq!(sanitized, format!("_{name}"));
        }
        assert_eq!(sanitize_filename("CON.txt"), "_CON.txt");
        assert_eq!(sanitize_filename("com1.txt"), "_com1.txt");
        assert_eq!(sanitize_filename("LPT9"), "_LPT9");
    }

    #[test]
    fn does_not_flag_names_that_merely_start_with_a_reserved_prefix() {
        assert_eq!(sanitize_filename("CONSTITUTION.txt"), "CONSTITUTION.txt");
        assert_eq!(sanitize_filename("COMPANY.pdf"), "COMPANY.pdf");
        assert_eq!(sanitize_filename("comedy.mp4"), "comedy.mp4");
        // COM0/LPT0 are not reserved (only 1-9).
        assert_eq!(sanitize_filename("COM0.txt"), "COM0.txt");
    }

    #[test]
    fn truncates_extremely_long_filenames_preserving_the_extension() {
        let long_stem = "a".repeat(500);
        let raw = format!("{long_stem}.pdf");
        let sanitized = sanitize_filename(&raw);
        assert!(sanitized.len() <= MAX_FILENAME_BYTES);
        assert!(sanitized.ends_with(".pdf"));
    }

    #[test]
    fn truncates_an_extremely_long_filename_with_no_extension() {
        let raw = "a".repeat(500);
        let sanitized = sanitize_filename(&raw);
        assert!(sanitized.len() <= MAX_FILENAME_BYTES);
        assert!(!sanitized.is_empty());
    }

    #[test]
    fn truncation_stays_on_a_utf8_char_boundary() {
        // Multi-byte characters near the truncation point must not be cut
        // through the middle of an encoded codepoint.
        let raw = format!("{}.txt", "あ".repeat(300));
        let sanitized = sanitize_filename(&raw);
        assert!(sanitized.len() <= MAX_FILENAME_BYTES);
        assert!(sanitized.ends_with(".txt"));
        // Re-parsing as UTF-8 must succeed (proves no boundary was cut).
        assert!(std::str::from_utf8(sanitized.as_bytes()).is_ok());
    }

    #[test]
    fn handles_a_dotfile_with_no_further_extension() {
        // split_once('.') on ".gitignore" would give an empty stem; the
        // whole name is kept intact rather than sanitized away.
        assert_eq!(sanitize_filename(".gitignore"), ".gitignore");
    }

    #[test]
    fn handles_a_filename_with_no_extension_at_all() {
        assert_eq!(sanitize_filename("README"), "README");
    }

    // --- unique_filename / build_destination: same-name collision ---

    #[test]
    fn unique_filename_is_unchanged_when_free() {
        assert_eq!(unique_filename("report.pdf", |_| false), "report.pdf");
    }

    #[test]
    fn unique_filename_appends_a_counter_on_collision() {
        let taken = ["report.pdf"];
        assert_eq!(
            unique_filename("report.pdf", |name| taken.contains(&name)),
            "report (1).pdf"
        );
    }

    #[test]
    fn unique_filename_advances_the_counter_until_free() {
        let taken = ["report.pdf", "report (1).pdf", "report (2).pdf"];
        assert_eq!(
            unique_filename("report.pdf", |name| taken.contains(&name)),
            "report (3).pdf"
        );
    }

    #[test]
    fn unique_filename_keeps_a_compound_extension_intact() {
        let taken = ["archive.tar.gz"];
        assert_eq!(
            unique_filename("archive.tar.gz", |name| taken.contains(&name)),
            "archive (1).tar.gz"
        );
    }

    #[test]
    fn unique_filename_handles_no_extension() {
        let taken = ["README"];
        assert_eq!(
            unique_filename("README", |name| taken.contains(&name)),
            "README (1)"
        );
    }

    #[test]
    fn unique_filename_handles_a_dotfile() {
        let taken = [".gitignore"];
        assert_eq!(
            unique_filename(".gitignore", |name| taken.contains(&name)),
            ".gitignore (1)"
        );
    }

    #[test]
    fn build_destination_sanitizes_then_avoids_collisions_on_a_real_directory() {
        let dir = unique_temp_dir("velox-downloads-collision");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("report.pdf"), b"existing").unwrap();

        let dest = build_destination(&dir, "../../etc/report.pdf");
        assert_eq!(dest, dir.join("report (1).pdf"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn prepare_destination_creates_missing_directories() {
        let dir = unique_temp_dir("velox-downloads-mkdir")
            .join("nested")
            .join("downloads");
        assert!(!dir.exists());
        let dest = prepare_destination(&dir, "report.pdf").expect("should create dir");
        assert_eq!(dest, dir.join("report.pdf"));
        assert!(dir.exists());

        std::fs::remove_dir_all(dir.parent().unwrap().parent().unwrap()).ok();
    }

    // --- download directory resolution (pure branch only — see D28) ---

    #[test]
    fn unix_dir_prefers_xdg_download_dir_when_set() {
        assert_eq!(
            resolve_unix_download_dir(Some("/custom/downloads"), Some("/home/alice")),
            Some(PathBuf::from("/custom/downloads"))
        );
    }

    #[test]
    fn unix_dir_falls_back_to_home_downloads() {
        assert_eq!(
            resolve_unix_download_dir(None, Some("/home/alice")),
            Some(PathBuf::from("/home/alice/Downloads"))
        );
    }

    #[test]
    fn unix_dir_is_none_without_xdg_or_home() {
        assert_eq!(resolve_unix_download_dir(None, None), None);
    }

    // --- open_path_command: pure command construction ---

    #[test]
    fn open_path_command_never_goes_through_a_shell() {
        let (program, args) = open_path_command(Path::new("/home/user/Downloads/report.pdf"));
        // Exercises whichever platform branch this is compiled for; on this
        // project's Linux-only CI (see CLAUDE.md) that is the xdg-open arm.
        assert!(!program.contains("sh"));
        assert_eq!(args, vec!["/home/user/Downloads/report.pdf".to_owned()]);
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    #[test]
    fn open_path_command_uses_xdg_open_on_linux() {
        let (program, _) = open_path_command(Path::new("/tmp/x"));
        assert_eq!(program, "xdg-open");
    }

    /// A per-test temp directory under the OS temp dir, distinguished by
    /// `label` plus the current thread so parallel tests never collide —
    /// same helper shape as `persistence`'s tests.
    fn unique_temp_dir(label: &str) -> PathBuf {
        let unique = format!(
            "{label}-{:?}-{}",
            std::thread::current().id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        );
        std::env::temp_dir().join(unique)
    }
}
