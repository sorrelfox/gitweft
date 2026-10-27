//! Git LFS: pointer files, whether a repository uses LFS, setting it up, fetching or
//! pulling the real contents, and file locks. Everything runs the user's `git lfs`
//! (git-lfs 3.x), so `lfs.url`, `.lfsconfig` and credential helpers behave as in a
//! terminal. A missing git-lfs isn't an error here: [`lfs_status`] reports it, and the
//! other calls fail like any command.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use gix::bstr::{BString, ByteSlice};

use crate::cancel::CancelToken;
use crate::cli::{GitCli, GitError, GitErrorKind, GitOutput, StderrEvent};
use crate::read::{FileDiff, LineKind};

/// The largest pointer file the spec allows.
pub const MAX_POINTER_SIZE: usize = 1024;

/// `version` values of spec v1 (the second is the pre-release name, still accepted).
const SPEC_VERSIONS: [&[u8]; 2] = [
    b"https://git-lfs.github.com/spec/v1",
    b"https://hawser.github.com/spec/v1",
];

/// What `git lfs pull` prints (exit 0) when the repository has no LFS filter: the
/// objects are downloaded but the work tree keeps the pointers.
const NOT_INSTALLED: &str = "Git LFS is not installed for this repository";

/// The content an LFS pointer file stands for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LfsPointer {
    /// The content's SHA-256: 64 lowercase hex digits.
    pub oid: String,
    /// The content's size in bytes.
    pub size: u64,
}

/// The content `pointer` stands for, read from this repository's local LFS store
/// (`<common git dir>/lfs/objects/<aa>/<bb>/<oid>`, shared by its worktrees): `None` when it
/// isn't there (or has another size), or is larger than `max_bytes`. Nothing is fetched:
/// `git lfs smudge` would download a missing object, so it isn't used (#36).
pub fn local_object(
    git: &GitCli,
    workdir: &Path,
    pointer: &LfsPointer,
    max_bytes: u64,
    cancel: &CancelToken,
) -> Result<Option<Vec<u8>>, GitError> {
    if pointer.size > max_bytes || pointer.oid.len() != 64 {
        return Ok(None);
    }
    let file = object_file(&local_store(git, workdir, cancel)?, &pointer.oid);
    match std::fs::read(&file) {
        Ok(bytes) if bytes.len() as u64 == pointer.size => Ok(Some(bytes)),
        Ok(_) => Ok(None),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(GitError::new(
            GitErrorKind::Other,
            "git lfs",
            format!("{}: {err}", file.display()),
        )),
    }
}

/// This repository's local LFS store, `<common git dir>/lfs/objects` (shared by its
/// worktrees).
fn local_store(git: &GitCli, workdir: &Path, cancel: &CancelToken) -> Result<PathBuf, GitError> {
    let output = git
        .command(workdir)
        .args(["rev-parse", "--git-common-dir"])
        .read_only()
        .run(cancel)?;
    let dir = output.stdout.strip_suffix(b"\n").unwrap_or(&output.stdout);
    Ok(workdir.join(OsStr::from_bytes(dir)).join("lfs/objects"))
}

/// Where the store keeps `oid` (64 hex digits): `<store>/<aa>/<bb>/<oid>`.
fn object_file(store: &Path, oid: &str) -> PathBuf {
    store.join(&oid[..2]).join(&oid[2..4]).join(oid)
}

/// Parses a pointer file (spec v1): at most [`MAX_POINTER_SIZE`] bytes of `key value`
/// lines, each ending in `\n`, `version <spec url>` first, with `oid sha256:<hex>` and
/// `size <decimal>` among the rest. Other keys (extensions) are allowed; anything else,
/// such as a text file that merely starts like a pointer, is not a pointer.
pub fn parse_pointer(bytes: &[u8]) -> Option<LfsPointer> {
    if bytes.len() > MAX_POINTER_SIZE {
        return None;
    }
    let mut lines = bytes.strip_suffix(b"\n")?.split(|b| *b == b'\n');
    let version = lines.next()?.strip_prefix(b"version ")?;
    if !SPEC_VERSIONS.contains(&version) {
        return None;
    }
    let (mut oid, mut size) = (None, None);
    let mut seen: Vec<&[u8]> = Vec::new();
    for line in lines {
        let space = line.iter().position(|b| *b == b' ')?;
        let (key, value) = (&line[..space], &line[space + 1..]);
        let key_ok = !key.is_empty()
            && key
                .iter()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'-'));
        if !key_ok || value.is_empty() || key == b"version" || seen.contains(&key) {
            return None;
        }
        seen.push(key);
        match key {
            b"oid" => oid = Some(parse_oid(value)?),
            b"size" => size = Some(parse_size(value)?),
            _ => {}
        }
    }
    Some(LfsPointer {
        oid: oid?,
        size: size?,
    })
}

/// `sha256:<64 lowercase hex>` → the hex.
fn parse_oid(value: &[u8]) -> Option<String> {
    let hex = value.strip_prefix(b"sha256:")?;
    let valid = hex.len() == 64
        && hex
            .iter()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b));
    valid.then(|| String::from_utf8_lossy(hex).into_owned())
}

/// Plain decimal digits only (`str::parse` would also take a leading `+`).
fn parse_size(value: &[u8]) -> Option<u64> {
    if !value.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(value).ok()?.parse().ok()
}

/// The pointers on the old and new side of a file's diff, when it's an LFS file: git
/// diffs the pointers (the clean filter turns work-tree content into one first), so a
/// changed LFS file shows as a three-line text diff of hashes and sizes.
///
/// `Some` only when every side that exists is a pointer: a new file has no old side
/// (`(None, Some(new))`), a deleted one no new side. A side counts as missing when the
/// diff shows no lines for it (`-0,0` / `+0,0`); since git-lfs never turns an empty file
/// into a pointer, an LFS file emptied in place therefore looks deleted. The hunks must
/// show the whole file from line 1 without gaps, as git's default three context lines
/// do for a pointer; otherwise (binary, truncated, a gap) the answer is `None`.
pub fn pointer_sides(diff: &FileDiff) -> Option<(Option<LfsPointer>, Option<LfsPointer>)> {
    if diff.binary || diff.truncated || diff.hunks.is_empty() {
        return None;
    }
    let (mut old, mut new) = (Vec::new(), Vec::new());
    // The line each side's next hunk must start at.
    let (mut old_next, mut new_next) = (1, 1);
    for hunk in &diff.hunks {
        let starts_right = |start: u32, len: u32, next: u32| len == 0 || start == next;
        if !starts_right(hunk.old_start, hunk.old_len, old_next)
            || !starts_right(hunk.new_start, hunk.new_len, new_next)
        {
            return None;
        }
        old_next += hunk.old_len;
        new_next += hunk.new_len;
        for line in &hunk.lines {
            let sides: &mut [&mut Vec<u8>] = match line.kind {
                LineKind::Context => &mut [&mut old, &mut new],
                LineKind::Removed => &mut [&mut old],
                LineKind::Added => &mut [&mut new],
            };
            for side in sides {
                side.extend_from_slice(&line.text);
                if !line.no_newline {
                    side.push(b'\n');
                }
            }
        }
    }
    // An empty old side of a file that isn't new is an empty file, not a missing one.
    if old.is_empty() && diff.new_file_mode.is_none() && !new.is_empty() {
        return None;
    }
    let side = |text: &[u8]| -> Option<Option<LfsPointer>> {
        if text.is_empty() {
            Some(None)
        } else {
            parse_pointer(text).map(Some)
        }
    };
    match (side(&old)?, side(&new)?) {
        (None, None) => None,
        sides => Some(sides),
    }
}

/// Whether and how a repository uses LFS.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LfsStatus {
    /// git-lfs's version (`3.7.1`), or `None` when it isn't installed.
    pub version: Option<String>,
    /// The LFS filter is configured (`filter.lfs.process` or `filter.lfs.smudge`, from
    /// `git lfs install` in any config file). Without it git checks out and commits the
    /// pointer files themselves.
    pub filter_configured: bool,
    /// Some file in the index is marked `filter=lfs` by `.gitattributes`.
    pub used: bool,
}

/// Whether git-lfs is installed, its filter configured, and the repository uses it.
/// Three quick commands (a few milliseconds even with 9 000 files), cheap enough for
/// every repository load.
pub fn lfs_status(
    git: &GitCli,
    workdir: &Path,
    cancel: &CancelToken,
) -> Result<LfsStatus, GitError> {
    // Without git-lfs: "git: 'lfs' is not a git command. See 'git --help'." (exit 1).
    let version = git
        .command(workdir)
        .args(["lfs", "version"])
        .allow_exit_codes(&[1])
        .read_only()
        .run(cancel)?;
    let version = (version.status == 0).then(|| parse_version(&version.stdout_str()));
    let filter = git
        .command(workdir)
        .args(["config", "--get-regexp", r"^filter\.lfs\.(process|smudge)$"])
        // Exit code 1: neither is set.
        .allow_exit_codes(&[1])
        .read_only()
        .run(cancel)?;
    // `attr:` is pathspec magic, which GIT_LITERAL_PATHSPECS turns off: the pathspec
    // would then match nothing, silently (exit 0).
    let used = git
        .command(workdir)
        .args(["ls-files", "-z", "--", ":(attr:filter=lfs)"])
        .env("GIT_LITERAL_PATHSPECS", "0")
        .read_only()
        .run(cancel)?;
    Ok(LfsStatus {
        version,
        filter_configured: filter.status == 0 && !filter.stdout.is_empty(),
        used: !used.stdout.is_empty(),
    })
}

/// `git-lfs/3.7.1 (GitHub; linux amd64; go 1.26.5)` → `3.7.1`; other text is kept whole.
fn parse_version(stdout: &str) -> String {
    let line = stdout.lines().next().unwrap_or_default().trim();
    line.strip_prefix("git-lfs/")
        .and_then(|rest| rest.split_whitespace().next())
        .unwrap_or(line)
        .to_owned()
}

/// A file stored in LFS, as `git lfs ls-files` lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LfsFile {
    pub path: BString,
    /// The content's SHA-256 (64 lowercase hex digits).
    pub oid: String,
    /// The content's size in bytes.
    pub size: u64,
    /// The content is in the local LFS store (`.git/lfs/objects`).
    pub downloaded: bool,
    /// The work-tree file holds the content, not the pointer.
    pub checked_out: bool,
}

/// The LFS files of the checked-out commit (`git lfs ls-files --json`, git-lfs 3.3+).
/// JSON can't carry non-UTF-8 names (git-lfs writes U+FFFD instead), so for those the
/// raw name comes from `--long`, whose lines are `<oid> <*|-> <name>` in the same order.
pub fn lfs_files(
    git: &GitCli,
    workdir: &Path,
    cancel: &CancelToken,
) -> Result<Vec<LfsFile>, GitError> {
    let command = git
        .command(workdir)
        .args(["lfs", "ls-files", "--json"])
        .read_only();
    let display = command.display();
    let output = command.run(cancel)?;
    let mut files = parse_ls_files_json(&output.stdout).ok_or_else(|| {
        GitError::new(
            GitErrorKind::Other,
            display,
            format!("unexpected output: {}", output.stdout_str()),
        )
    })?;
    if files.iter().any(|file| file.path.contains_str("\u{fffd}")) {
        let long = git
            .command(workdir)
            .args(["lfs", "ls-files", "--long"])
            .read_only()
            .run(cancel)?;
        restore_raw_names(&mut files, &long.stdout);
    }
    Ok(files)
}

fn parse_ls_files_json(stdout: &[u8]) -> Option<Vec<LfsFile>> {
    #[derive(serde::Deserialize)]
    struct Listing {
        /// `null` when there are none.
        files: Option<Vec<Entry>>,
    }
    #[derive(serde::Deserialize)]
    struct Entry {
        name: String,
        oid: String,
        size: u64,
        #[serde(default)]
        downloaded: bool,
        #[serde(default)]
        checkout: bool,
    }
    let listing: Listing = serde_json::from_slice(stdout).ok()?;
    Some(
        listing
            .files
            .unwrap_or_default()
            .into_iter()
            .map(|entry| LfsFile {
                path: entry.name.into(),
                oid: entry.oid,
                size: entry.size,
                downloaded: entry.downloaded,
                checked_out: entry.checkout,
            })
            .collect(),
    )
}

/// Replaces lossy JSON names with the raw ones from `--long` output, line by line, where
/// the oid and the lossy spelling agree (a name with a newline would shift the lines;
/// then the lossy name stays).
fn restore_raw_names(files: &mut [LfsFile], long: &[u8]) {
    let lines = long
        .strip_suffix(b"\n")
        .unwrap_or(long)
        .split(|b| *b == b'\n');
    for (file, line) in files.iter_mut().zip(lines) {
        // `<64 hex> <marker> <name>`
        let (Some(oid), Some(name)) = (line.get(..64), line.get(67..)) else {
            continue;
        };
        if oid == file.oid.as_bytes() && name.to_str_lossy() == file.path.to_str_lossy() {
            file.path = name.into();
        }
    }
}

/// Downloads the LFS objects the current branch needs into `.git/lfs/objects`, leaving
/// the work tree alone (`git lfs fetch`). Progress arrives as `Downloading LFS objects`
/// events.
pub fn lfs_fetch(
    git: &GitCli,
    workdir: &Path,
    cancel: &CancelToken,
    on_event: impl FnMut(StderrEvent),
) -> Result<GitOutput, GitError> {
    transfer(git, workdir, &["fetch"], cancel, on_event)
}

/// Downloads the LFS objects of every commit that any ref reaches (`git lfs fetch --all`),
/// so older versions and other branches can be checked out offline. The work tree stays
/// as it is; progress as in [`lfs_fetch`].
pub fn lfs_fetch_all(
    git: &GitCli,
    workdir: &Path,
    cancel: &CancelToken,
    on_event: impl FnMut(StderrEvent),
) -> Result<GitOutput, GitError> {
    transfer(git, workdir, &["fetch", "--all"], cancel, on_event)
}

/// Local LFS copies that `git lfs prune` deletes, or deleted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LfsPruned {
    /// Their oids.
    pub oids: Vec<String>,
    /// Their total size in the local store, in bytes.
    pub bytes: u64,
}

/// What `git lfs prune` would delete (`--dry-run --verbose`): local copies that neither the
/// checkout, a stash, a recent branch or commit, an unpushed commit nor another worktree
/// needs. The sizes are the store's files' own (git-lfs rounds its figures).
pub fn lfs_prunable(
    git: &GitCli,
    workdir: &Path,
    cancel: &CancelToken,
) -> Result<LfsPruned, GitError> {
    // Off a terminal git-lfs writes only the last line of its listing (a meter line),
    // unless its progress is forced.
    let output = git
        .command(workdir)
        .args(["lfs", "prune", "--dry-run", "--verbose"])
        .env("GIT_LFS_FORCE_PROGRESS", "1")
        .read_only()
        .run(cancel)?;
    let store = local_store(git, workdir, cancel)?;
    let text = format!("{}\n{}", output.stdout_str(), output.stderr);
    let oids = parse_prune_list(&text);
    let bytes = oids
        .iter()
        .filter_map(|oid| std::fs::metadata(object_file(&store, oid)).ok())
        .map(|meta| meta.len())
        .sum();
    Ok(LfsPruned { oids, bytes })
}

/// `git lfs prune`: deletes the local copies [`lfs_prunable`] lists. Returns those that
/// are gone afterwards, with their sizes as they were; the remote keeps them, but they
/// can't be had back offline.
pub fn lfs_prune(
    git: &GitCli,
    workdir: &Path,
    cancel: &CancelToken,
) -> Result<LfsPruned, GitError> {
    let before = lfs_prunable(git, workdir, cancel)?;
    let store = local_store(git, workdir, cancel)?;
    let sizes: Vec<(String, u64)> = before
        .oids
        .into_iter()
        .map(|oid| {
            let size = std::fs::metadata(object_file(&store, &oid)).map_or(0, |meta| meta.len());
            (oid, size)
        })
        .collect();
    git.command(workdir).args(["lfs", "prune"]).run(cancel)?;
    let gone: Vec<_> = sizes
        .into_iter()
        .filter(|(oid, _)| !object_file(&store, oid).exists())
        .collect();
    Ok(LfsPruned {
        bytes: gone.iter().map(|(_, size)| size).sum(),
        oids: gone.into_iter().map(|(oid, _)| oid).collect(),
    })
}

/// The oids in `git lfs prune --dry-run --verbose` output: ` * <oid> (<size>)` lines, among
/// progress lines that end in `\r` rather than `\n`; the meter repeats the last one (`…,
/// done.`), so each oid is taken once.
fn parse_prune_list(stdout: &str) -> Vec<String> {
    let mut oids: Vec<String> = Vec::new();
    let listed = stdout
        .split(['\r', '\n'])
        .filter_map(|line| line.trim_start().strip_prefix("* "))
        .filter_map(|rest| rest.split_whitespace().next())
        .filter(|oid| oid.len() == 64 && oid.bytes().all(|b| b.is_ascii_hexdigit()));
    for oid in listed {
        if !oids.iter().any(|seen| seen == oid) {
            oids.push(oid.to_owned());
        }
    }
    oids
}

/// A kind of file in [`lfs_migrate_info`]'s report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LfsTypeSize {
    /// `*.bin`, or a whole name for files without an extension.
    pub pattern: String,
    /// The size of all its versions in the history examined.
    pub bytes: u64,
    /// How many versions (blobs) of such files there are.
    pub files: u64,
}

/// Which kinds of files take the most room in plain git, largest first (#71): what `git
/// lfs migrate import` could move into LFS. Only reads: `git lfs migrate info`, over the
/// checked-out branch's history, or with `everything` every local and remote branch and
/// tag. Files already in LFS are left out (`--pointers=ignore`). At most `top` kinds.
pub fn lfs_migrate_info(
    git: &GitCli,
    workdir: &Path,
    everything: bool,
    top: usize,
    cancel: &CancelToken,
) -> Result<Vec<LfsTypeSize>, GitError> {
    let mut command =
        git.command(workdir)
            .args(["lfs", "migrate", "info", "--unit=b", "--pointers=ignore"]);
    command = command.arg(format!("--top={top}"));
    if everything {
        command = command.arg("--everything");
    }
    let output = command.read_only().run(cancel)?;
    Ok(parse_migrate_info(&output.stdout_str()))
}

/// `*.bin<TAB>305000<TAB>2/2 files<TAB>100%` lines (sizes in bytes with `--unit=b`, padded
/// with spaces); other lines are skipped.
fn parse_migrate_info(stdout: &str) -> Vec<LfsTypeSize> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t').map(str::trim);
            let pattern = fields.next().filter(|p| !p.is_empty())?;
            let bytes = fields.next()?.parse().ok()?;
            // "2/2 files": those above `--above` (none is set) of all of the kind.
            let files = fields.next()?.split_whitespace().next()?;
            let files = files.split('/').next_back()?.parse().ok()?;
            Some(LfsTypeSize {
                pattern: pattern.to_owned(),
                bytes,
                files,
            })
        })
        .collect()
}

/// Which way `git lfs migrate` moves files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrateDirection {
    /// Into LFS: the files become pointers, their contents go to the LFS store.
    Import,
    /// Out of LFS: the pointers become the files again.
    Export,
}

/// The patterns a user typed (`*.psd, *.zip` or one per word), as `--include` takes them.
pub fn migrate_patterns(text: &str) -> Vec<String> {
    text.split(|c: char| c == ',' || c.is_whitespace())
        .map(str::trim)
        .filter(|pattern| !pattern.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Rewrites the history of `branch` (a full ref name, the checked-out branch's: the only
/// scope the app offers, #71) so that the files matching `patterns` are kept in LFS, or
/// no longer are: `git lfs migrate import|export --include=… --include-ref=<branch>`. Every
/// commit on it that has such a file gets a new id; other branches and tags are left as
/// they are. The work tree must be clean (git-lfs asks otherwise, and stdin is empty, so
/// it refuses). Pushing the result needs a force push.
pub fn lfs_migrate(
    git: &GitCli,
    workdir: &Path,
    direction: MigrateDirection,
    patterns: &[String],
    branch: &str,
    cancel: &CancelToken,
) -> Result<(), GitError> {
    let verb = match direction {
        MigrateDirection::Import => "import",
        MigrateDirection::Export => "export",
    };
    git.command(workdir)
        .args(["lfs", "migrate", verb])
        .arg(format!("--include={}", patterns.join(",")))
        .arg(format!("--include-ref={branch}"))
        .run(cancel)?;
    Ok(())
}

/// Downloads the current branch's LFS objects and replaces the pointer files in the work
/// tree with their contents (`git lfs pull`). Without the LFS filter git-lfs downloads
/// but skips the checkout, exiting 0; that is reported as an error.
pub fn lfs_pull(
    git: &GitCli,
    workdir: &Path,
    cancel: &CancelToken,
    on_event: impl FnMut(StderrEvent),
) -> Result<GitOutput, GitError> {
    let output = transfer(git, workdir, &["pull"], cancel, on_event)?;
    let stdout = output.stdout_str();
    let skipped = stdout
        .split(['\r', '\n'])
        .chain(output.stderr.lines())
        .find(|line| line.contains(NOT_INSTALLED))
        .map(str::trim);
    if let Some(line) = skipped {
        return Err(GitError {
            kind: GitErrorKind::Other,
            command: "git lfs pull".to_owned(),
            stderr: format!("error: {line}"),
            exit_code: Some(output.status),
        });
    }
    Ok(output)
}

/// git-lfs shows its meter only on a terminal unless forced, and writes it to stdout.
fn transfer(
    git: &GitCli,
    workdir: &Path,
    args: &[&str],
    cancel: &CancelToken,
    on_event: impl FnMut(StderrEvent),
) -> Result<GitOutput, GitError> {
    git.command(workdir)
        .arg("lfs")
        .args(args)
        .env("GIT_LFS_FORCE_PROGRESS", "1")
        .progress_on_stdout()
        .run_with_events(cancel, on_event)
}

/// Sets up LFS for this repository only (`git lfs install --local`): the filter in
/// `.git/config`, after which [`lfs_status`] reports `filter_configured`, and the
/// `pre-push`, `post-checkout`, `post-commit` and `post-merge` hooks. A hook of the
/// repository's own is kept: git-lfs then fails (exit 2, `Hook already exists: <name>`),
/// though the filter is set by then.
pub fn lfs_install_local(
    git: &GitCli,
    workdir: &Path,
    cancel: &CancelToken,
) -> Result<(), GitError> {
    git.command(workdir)
        .args(["lfs", "install", "--local"])
        .run(cancel)?;
    Ok(())
}

/// A file lock on the LFS server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LfsLock {
    /// The server's id for the lock.
    pub id: String,
    /// The locked file, relative to the repository root. It comes from JSON, so a
    /// non-UTF-8 name has U+FFFD in it.
    pub path: BString,
    /// Who holds it, as the server names them; empty when it doesn't say.
    pub owner: String,
    /// When it was taken, in seconds since the Unix epoch.
    pub locked_at: Option<i64>,
}

/// The locks the LFS server holds (`git lfs locks --json`), which asks the server.
///
/// A remote without a locking API fails with [`GitErrorKind::LfsLockingUnsupported`]. For
/// a `file://` remote git-lfs itself only prints a hint (`The remote resolves to a file://
/// URL, which can only work with a standalone transfer agent`) and lists no locks, exit 0,
/// which would read as "nobody holds a lock". An HTTP server answering 404 (no locking API)
/// gets the same empty list and exit 0 without any message: only git-lfs's HTTP trace
/// shows the 404, so the trace goes to a file of our own (`GIT_TRACE`) and is read after.
/// With no remote at all it lists none.
pub fn lfs_locks(
    git: &GitCli,
    workdir: &Path,
    cancel: &CancelToken,
) -> Result<Vec<LfsLock>, GitError> {
    static TRACES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let serial = TRACES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let trace =
        std::env::temp_dir().join(format!("gitweft-lfs-trace-{}-{serial}", std::process::id()));
    let command = git
        .command(workdir)
        .args(["lfs", "locks", "--json"])
        .env("GIT_TRACE", &trace);
    let display = command.display();
    let result = command.run(cancel);
    let traced = std::fs::read_to_string(&trace).unwrap_or_default();
    let _ = std::fs::remove_file(&trace);
    let output = result.map_err(locking_error)?;
    if answered_not_found(&traced) {
        return Err(GitError {
            kind: GitErrorKind::LfsLockingUnsupported,
            command: display,
            stderr: format!("{NO_LOCKING_API_LINE} (the server answered 404 Not Found)"),
            exit_code: Some(output.status),
        });
    }
    if locking_unsupported(&output.stderr) {
        return Err(locking_error(GitError {
            kind: GitErrorKind::Other,
            command: display,
            stderr: output.stderr,
            exit_code: Some(output.status),
        }));
    }
    parse_locks_json(&output.stdout).ok_or_else(|| unexpected_output(display, &output))
}

/// Locks `path` (relative to the repository root) on the LFS server (`git lfs lock --json
/// -- <path>`) and returns the lock. Fails when someone holds it already, and with
/// [`GitErrorKind::LfsLockingUnsupported`] for a `file://` remote (`Locking <path> failed:
/// missing protocol: "file://…"`).
pub fn lfs_lock(
    git: &GitCli,
    workdir: &Path,
    path: &[u8],
    cancel: &CancelToken,
) -> Result<LfsLock, GitError> {
    let command = git
        .command(workdir)
        .args(["lfs", "lock", "--json", "--"])
        .arg(OsStr::from_bytes(path));
    let display = command.display();
    let output = command.run(cancel).map_err(locking_error)?;
    parse_lock_json(&output.stdout).ok_or_else(|| unexpected_output(display, &output))
}

/// Releases the lock on `path` (`git lfs unlock [--force] -- <path>`). `force` breaks
/// someone else's lock, where the server allows it; without it git-lfs also refuses when
/// the file has uncommitted changes.
pub fn lfs_unlock(
    git: &GitCli,
    workdir: &Path,
    path: &[u8],
    force: bool,
    cancel: &CancelToken,
) -> Result<(), GitError> {
    let mut command = git.command(workdir).args(["lfs", "unlock"]);
    if force {
        command = command.arg("--force");
    }
    command
        .arg("--")
        .arg(OsStr::from_bytes(path))
        .run(cancel)
        .map_err(locking_error)?;
    Ok(())
}

/// git-lfs's hint, on every lock command, when the remote is a `file://` URL.
const NO_LOCKING_API: &str = "The remote resolves to a file:// URL";

/// The first line of a lock command's error when the remote has no locking API.
const NO_LOCKING_API_LINE: &str = "error: this LFS remote has no file-locking API";

/// A `file://` remote's hint, or a lock / unlock refused with the server's 404 (`Locking
/// a.bin failed: Not Found`).
fn locking_unsupported(stderr: &str) -> bool {
    stderr.contains(NO_LOCKING_API) || stderr.trim_end().ends_with(": Not Found")
}

/// Whether git-lfs's trace (`GIT_TRACE`) shows a request for the locks answered 404:
/// `trace git-lfs: HTTP: GET <url>/locks` then `trace git-lfs: HTTP: 404`.
fn answered_not_found(trace: &str) -> bool {
    let mut asking_locks = false;
    for line in trace.lines() {
        let Some((_, http)) = line.split_once("trace git-lfs: HTTP: ") else {
            continue;
        };
        if let Some(target) = http
            .strip_prefix("GET ")
            .or_else(|| http.strip_prefix("POST "))
        {
            asking_locks = target.contains("/locks");
        } else if asking_locks && http.trim() == "404" {
            return true;
        }
    }
    false
}

/// Marks a lock command's failure that comes from a remote without a locking API, with a
/// line that says so first (git-lfs's own last line is the end of its hint).
fn locking_error(mut err: GitError) -> GitError {
    if err.kind != GitErrorKind::Cancelled && locking_unsupported(&err.stderr) {
        err.kind = GitErrorKind::LfsLockingUnsupported;
        err.stderr = format!("{NO_LOCKING_API_LINE}\n{}", err.stderr);
    }
    err
}

fn unexpected_output(command: String, output: &GitOutput) -> GitError {
    GitError::new(
        GitErrorKind::Other,
        command,
        format!("unexpected output: {}", output.stdout_str()),
    )
}

/// A lock as git-lfs writes it (`locking.Lock`, Go's JSON encoding): `owner` may be left
/// out, `locked_at` is RFC 3339.
#[derive(serde::Deserialize)]
struct JsonLock {
    id: String,
    path: String,
    #[serde(default)]
    owner: Option<JsonOwner>,
    #[serde(default)]
    locked_at: Option<String>,
}

#[derive(serde::Deserialize)]
struct JsonOwner {
    #[serde(default)]
    name: String,
}

impl From<JsonLock> for LfsLock {
    fn from(lock: JsonLock) -> Self {
        Self {
            id: lock.id,
            path: lock.path.into(),
            owner: lock.owner.map(|owner| owner.name).unwrap_or_default(),
            locked_at: lock
                .locked_at
                .as_deref()
                // Go's zero time: not set.
                .filter(|at| !at.starts_with("0001-01-01T00:00:00"))
                .and_then(parse_rfc3339),
        }
    }
}

/// `git lfs locks --json`: an array (`null` from Go for none; nothing at all is none too).
fn parse_locks_json(stdout: &[u8]) -> Option<Vec<LfsLock>> {
    if stdout.trim_ascii().is_empty() {
        return Some(Vec::new());
    }
    let locks: Option<Vec<JsonLock>> = serde_json::from_slice(stdout).ok()?;
    Some(
        locks
            .unwrap_or_default()
            .into_iter()
            .map(LfsLock::from)
            .collect(),
    )
}

/// `git lfs lock --json`: one lock, which git-lfs 3.x writes as a list of one (older
/// versions as the object itself).
fn parse_lock_json(stdout: &[u8]) -> Option<LfsLock> {
    let lock = serde_json::from_slice::<JsonLock>(stdout)
        .ok()
        .or_else(|| {
            let mut list = serde_json::from_slice::<Vec<JsonLock>>(stdout).ok()?;
            (list.len() == 1).then(|| list.remove(0))
        })?;
    Some(LfsLock::from(lock))
}

/// `2016-05-17T15:49:06+00:00`, `2024-02-29T23:59:59.123456789Z` (Go's RFC 3339 with
/// optional fractions, which are dropped) → seconds since the Unix epoch.
fn parse_rfc3339(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    let number = |from: usize, to: usize| -> Option<i64> {
        let digits = bytes.get(from..to)?;
        if !digits.iter().all(u8::is_ascii_digit) {
            return None;
        }
        std::str::from_utf8(digits).ok()?.parse().ok()
    };
    let separators = [(4, b'-'), (7, b'-'), (13, b':'), (16, b':')];
    if separators
        .iter()
        .any(|&(at, sep)| bytes.get(at) != Some(&sep))
        || !matches!(bytes.get(10), Some(b'T' | b't' | b' '))
    {
        return None;
    }
    let (year, month, day) = (number(0, 4)?, number(5, 7)?, number(8, 10)?);
    let (hour, minute, second) = (number(11, 13)?, number(14, 16)?, number(17, 19)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 {
        return None;
    }
    // Everything before is ASCII, so 19 is a char boundary.
    let mut rest = &text[19..];
    if let Some(fraction) = rest.strip_prefix('.') {
        let digits = fraction.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        rest = &fraction[digits..];
    }
    let offset = match rest.as_bytes() {
        [b'Z' | b'z'] => 0,
        [sign @ (b'+' | b'-'), h1, h2, b':', m1, m2] => {
            let two = |a: u8, b: u8| -> Option<i64> {
                (a.is_ascii_digit() && b.is_ascii_digit())
                    .then(|| i64::from((a - b'0') * 10 + b - b'0'))
            };
            let seconds = two(*h1, *h2)? * 3600 + two(*m1, *m2)? * 60;
            if *sign == b'-' { -seconds } else { seconds }
        }
        _ => return None,
    };
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second - offset)
}

/// Days from 1970-01-01 to a date of the proleptic Gregorian calendar (Howard Hinnant's
/// `days_from_civil`).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    #[test]
    fn migrate_info_rows_are_read_in_bytes() {
        let out = "*.bin\t305000\t2/2 files\t100%\n*.png\t70000 \t1/1 file \t100%\n\
                   noext\t2     \t1/1 file \t100%\nSorting commits: ..., done.\n";
        let rows = super::parse_migrate_info(out);
        let row = |pattern: &str, bytes, files| super::LfsTypeSize {
            pattern: pattern.to_owned(),
            bytes,
            files,
        };
        assert_eq!(
            rows,
            [
                row("*.bin", 305_000, 2),
                row("*.png", 70_000, 1),
                row("noext", 2, 1)
            ]
        );
        assert!(super::parse_migrate_info("").is_empty());
    }

    use super::*;

    #[test]
    fn a_404_for_the_locks_is_found_in_git_lfs_trace() {
        let trace = "11:10:31.138 git.c:811 trace: exec: git-lfs locks --json\n\
            11:10:31.147 trace git-lfs: HTTP: GET http://127.0.0.1:1/repo.git/info/lfs/locks\n\
            11:10:31.147 trace git-lfs: HTTP: 404\n\
            11:10:31.147 trace git-lfs: HTTP: {\"message\":\"Not Found\"}\n";
        assert!(answered_not_found(trace));
        // Found locks, or a 404 for something else (an object), aren't.
        assert!(!answered_not_found(
            &trace.replace("HTTP: 404", "HTTP: 200")
        ));
        let other = trace.replace("/locks", "/objects/batch");
        assert!(!answered_not_found(&other));
        assert!(!answered_not_found(""));
        assert!(locking_unsupported("Locking a.bin failed: Not Found\n"));
        assert!(!locking_unsupported("lock is owned by Someone Else"));
    }
    use crate::read::parse_unified;

    const OID: &str = "4859402c258b836d02e955d1090e29f586e58b2040504d68afec3d8d43757bba";

    fn pointer(oid: &str, size: u64) -> String {
        format!("version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize {size}\n")
    }

    #[test]
    fn pointers_parse() {
        let parsed = parse_pointer(pointer(OID, 10).as_bytes()).unwrap();
        assert_eq!((parsed.oid.as_str(), parsed.size), (OID, 10));
        // The pre-release spec URL.
        let legacy =
            format!("version https://hawser.github.com/spec/v1\noid sha256:{OID}\nsize 0\n");
        assert_eq!(parse_pointer(legacy.as_bytes()).unwrap().size, 0);
        // Extension keys are fine.
        let extended = format!(
            "version https://git-lfs.github.com/spec/v1\n\
             ext-0-foo sha256:{OID}\noid sha256:{OID}\nsize 12345678901\n"
        );
        assert_eq!(
            parse_pointer(extended.as_bytes()).unwrap().size,
            12_345_678_901
        );
    }

    #[test]
    fn near_misses_are_not_pointers() {
        let good = pointer(OID, 10);
        let upper = pointer(&OID.to_uppercase(), 10);
        let short = pointer(&OID[..63], 10);
        let too_long = format!("{good}x-pad {}\n", "a".repeat(MAX_POINTER_SIZE));
        let cases = [
            // A text file that mentions the version line.
            "version https://git-lfs.github.com/spec/v1\nhello\n".to_owned(),
            good.trim_end().to_owned(),           // no final newline
            good.replace('\n', "\r\n"),           // CRLF
            good.replace("size 10", "size +10"),  // not plain decimal
            good.replace("size 10\n", ""),        // no size
            good.replace("sha256:", "sha1:"),     // other hash
            good.replace("/spec/v1", "/spec/v2"), // unknown version
            format!("{good}size 11\n"),           // duplicate key
            format!("{good}\n"),                  // empty line
            format!("oid sha256:{OID}\n{good}"),  // version not first
            upper,
            short,
            too_long,
            String::new(),
        ];
        for case in cases {
            assert_eq!(parse_pointer(case.as_bytes()), None, "{case:?}");
        }
    }

    fn diff(text: &str) -> FileDiff {
        parse_unified(text.as_bytes()).swap_remove(0)
    }

    #[test]
    fn pointer_sides_need_the_whole_file_on_each_side() {
        let other = "27dd8ed44a83ff94d557f9fd0412ed5a8cbca69ea04922d88c01184a07300a5a";
        let changed = diff(&format!(
            "diff --git a/a.bin b/a.bin\n--- a/a.bin\n+++ b/a.bin\n@@ -1,3 +1,3 @@\n \
             version https://git-lfs.github.com/spec/v1\n-oid sha256:{OID}\n-size 4\n\
             +oid sha256:{other}\n+size 6\n"
        ));
        let (old, new) = pointer_sides(&changed).unwrap();
        assert_eq!(old.unwrap().size, 4);
        assert_eq!(new.unwrap().oid, other);

        // A hunk that starts further down doesn't show the whole pointer.
        let partial = diff(&format!(
            "diff --git a/a.bin b/a.bin\n--- a/a.bin\n+++ b/a.bin\n@@ -2,2 +2,2 @@\n\
             -oid sha256:{OID}\n-size 4\n+oid sha256:{other}\n+size 6\n"
        ));
        assert_eq!(pointer_sides(&partial), None);

        // "No newline at end of file" on the new side: not a valid pointer there.
        let unterminated = diff(&format!(
            "diff --git a/a.bin b/a.bin\n--- a/a.bin\n+++ b/a.bin\n@@ -1,3 +1,3 @@\n \
             version https://git-lfs.github.com/spec/v1\n oid sha256:{OID}\n-size 4\n+size 4\n\
             \\ No newline at end of file\n"
        ));
        assert_eq!(pointer_sides(&unterminated), None);

        // An empty file that gains pointer content wasn't a pointer before.
        let from_empty = diff(&format!(
            "diff --git a/a.bin b/a.bin\n--- a/a.bin\n+++ b/a.bin\n@@ -0,0 +1,3 @@\n{}",
            pointer(OID, 4)
                .lines()
                .map(|line| format!("+{line}\n"))
                .collect::<String>()
        ));
        assert_eq!(pointer_sides(&from_empty), None);
        let added = FileDiff {
            new_file_mode: Some("100644".into()),
            ..from_empty
        };
        assert_eq!(pointer_sides(&added).unwrap().0, None);
    }

    #[test]
    fn ls_files_json_and_raw_names() {
        let other = "2e7d2c03a9507ae265ecf5b5356885a53393a2029d241394997265a1a25aefc6";
        let json = format!(
            r#"{{"files": [
                {{"name": "bad�.bin", "size": 1, "checkout": false, "downloaded": true,
                  "oid_type": "sha256", "oid": "{OID}", "version": "https://git-lfs.github.com/spec/v1"}},
                {{"name": "dir/a b.bin", "size": 3000000, "checkout": true, "downloaded": true,
                  "oid_type": "sha256", "oid": "{other}", "version": "https://git-lfs.github.com/spec/v1"}}
            ]}}"#
        );
        let mut files = parse_ls_files_json(json.as_bytes()).unwrap();
        assert_eq!(files[1].path, "dir/a b.bin");
        assert_eq!((files[1].size, files[1].checked_out), (3_000_000, true));
        assert!(files[0].downloaded && !files[0].checked_out);

        let mut long = format!("{OID} - bad").into_bytes();
        long.extend_from_slice(b"\xff.bin\n");
        long.extend_from_slice(format!("{other} * dir/a b.bin\n").as_bytes());
        restore_raw_names(&mut files, &long);
        assert_eq!(files[0].path, &b"bad\xff.bin"[..]);
        assert_eq!(files[1].path, "dir/a b.bin");

        assert_eq!(parse_ls_files_json(br#"{"files": null}"#), Some(Vec::new()));
        assert_eq!(parse_ls_files_json(b"not json"), None);
    }

    #[test]
    fn prune_lists_oids_among_progress_lines() {
        let (a, b) = ("a".repeat(64), "0123456789abcdef".repeat(4));
        let stdout = format!(
            "4 local objects, 1 retained\r4 local objects, 1 retained, done.\n\
             2 files would be pruned (11 B)   \r\n * {a} (3 B)   \r\n * {b} (8 B)   \r\n\
             * {b} (8 B), done.\n * short (1 B)\n"
        );
        assert_eq!(parse_prune_list(&stdout), [a, b]);
        assert!(parse_prune_list("4 local objects, 4 retained, done.\n").is_empty());
    }

    #[test]
    fn versions() {
        assert_eq!(
            parse_version("git-lfs/3.7.1 (GitHub; linux amd64; go 1.26.5-X:nodwarf5)\n"),
            "3.7.1"
        );
        assert_eq!(parse_version("something else\n"), "something else");
    }

    /// The shape of git-lfs's `locking.Lock` (docs/api/locking.md), as `locks --json`
    /// writes it.
    #[test]
    fn locks_json() {
        let json = r#"[
            {"id": "1", "path": "assets/big.psd", "owner": {"name": "Jane Doe"},
             "locked_at": "2016-05-17T15:49:06+00:00"},
            {"id": "2", "path": "café.bin", "locked_at": "2024-02-29T23:59:59.123456789Z"},
            {"id": "3", "path": "x.bin", "owner": {"name": "x"}, "locked_at": "0001-01-01T00:00:00Z"}
        ]"#;
        let lock = |id: &str, path: &str, owner: &str, locked_at: Option<i64>| LfsLock {
            id: id.into(),
            path: path.into(),
            owner: owner.into(),
            locked_at,
        };
        assert_eq!(
            parse_locks_json(json.as_bytes()).unwrap(),
            [
                lock("1", "assets/big.psd", "Jane Doe", Some(1_463_500_146)),
                lock("2", "café.bin", "", Some(1_709_251_199)),
                lock("3", "x.bin", "x", None),
            ]
        );
        assert_eq!(parse_locks_json(b"[]\n"), Some(Vec::new()));
        assert_eq!(parse_locks_json(b"null\n"), Some(Vec::new()));
        assert_eq!(parse_locks_json(b""), Some(Vec::new()));
        assert_eq!(parse_locks_json(b"{\"ours\": []}"), None);
        assert_eq!(parse_locks_json(b"not json"), None);

        let one = br#"{"id":"7","path":"a.bin","owner":{"name":"me"},"locked_at":"2016-05-17T17:49:06+02:00"}"#;
        assert_eq!(
            parse_lock_json(one),
            Some(lock("7", "a.bin", "me", Some(1_463_500_146)))
        );
        assert_eq!(parse_lock_json(b"[]"), None);
    }

    #[test]
    fn rfc3339_times() {
        for (text, expected) in [
            ("1970-01-01T00:00:00Z", Some(0)),
            ("2016-05-17T15:49:06+00:00", Some(1_463_500_146)),
            ("2016-05-17T10:49:06-05:00", Some(1_463_500_146)),
            ("2016-05-17t15:49:06.5z", Some(1_463_500_146)),
            ("1969-12-31T23:59:59Z", Some(-1)),
            ("2016-05-17T15:49:06", None),
            ("2016-05-17T15:49:06.Z", None),
            ("2016-13-17T15:49:06Z", None),
            ("2016-05-17T15:49:06+0000", None),
            ("2016/05/17T15:49:06Z", None),
            ("", None),
        ] {
            assert_eq!(parse_rfc3339(text), expected, "{text}");
        }
    }

    #[test]
    fn a_file_remote_has_no_locking_api() {
        let hint = "hint: The remote resolves to a file:// URL, which can only work with a\n\
                    hint: standalone transfer agent.  See section \"Using a Custom Transfer Type\n\
                    hint: without the API server\" in custom-transfers.md for details.\n\
                    Locking a.bin failed: missing protocol: \"file:///tmp/remote.git\"";
        let err = locking_error(GitError::new(GitErrorKind::Other, "git lfs lock", hint));
        assert_eq!(err.kind, GitErrorKind::LfsLockingUnsupported);
        assert_eq!(
            err.summary(),
            "error: this LFS remote has no file-locking API"
        );
        let other = locking_error(GitError::new(
            GitErrorKind::AuthFailed,
            "git lfs lock",
            "error: Authentication required",
        ));
        assert_eq!(other.kind, GitErrorKind::AuthFailed);
    }
}
