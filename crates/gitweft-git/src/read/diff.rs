//! Unified diffs of single files, parsed for display and (later) partial staging.
//!
//! Diffs come from `git diff` so they match what git itself would apply: textconv,
//! filters, rename detection and `diff.*` settings all behave like in a terminal.
//! Line text is kept as raw bytes so patches can be rebuilt exactly.

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use gix::ObjectId;
use gix::bstr::{BString, ByteSlice};

use super::ReadError;
use crate::cancel::CancelToken;
use crate::cli::{GitCli, GitError};

/// Diffs longer than this are cut (with [`FileDiff::truncated`] set).
pub const MAX_DIFF_LINES: usize = 50_000;
/// Untracked files larger than this are not read.
const MAX_UNTRACKED_BYTES: u64 = 8 * 1024 * 1024;
/// A context size that shows the whole file.
pub const FULL_CONTEXT: u32 = 1_000_000;

/// How a diff is computed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiffOptions {
    /// Unchanged lines shown around each change (git's `-U`). Larger values merge
    /// nearby hunks; patches built from them still apply.
    pub context: u32,
    /// Ignore all whitespace changes (`-w`). Such a diff no longer matches the file, so
    /// it can't be used to build patches.
    pub ignore_whitespace: bool,
}

impl Default for DiffOptions {
    fn default() -> Self {
        Self {
            context: 3,
            ignore_whitespace: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Added,
    Removed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: LineKind,
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
    /// Line content without the leading `+`/`-`/` ` and without the trailing newline.
    pub text: BString,
    /// The line is followed by `\ No newline at end of file`.
    pub no_newline: bool,
}

impl DiffLine {
    /// Text for display: lossy UTF-8, carriage return removed, tabs expanded to tab stops
    /// `tab` columns apart ([`crate::display::expand_tabs`]).
    pub fn display_text(&self, tab: usize) -> String {
        let text = self.text.to_str_lossy();
        crate::display::expand_tabs(text.trim_end_matches('\r'), tab).into_owned()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub old_start: u32,
    pub old_len: u32,
    pub new_start: u32,
    pub new_len: u32,
    /// Function context git prints after the second `@@`.
    pub section: String,
    pub lines: Vec<DiffLine>,
}

impl Hunk {
    pub fn header(&self) -> String {
        let section = if self.section.is_empty() {
            String::new()
        } else {
            format!(" {}", self.section)
        };
        format!(
            "@@ -{},{} +{},{} @@{section}",
            self.old_start, self.old_len, self.new_start, self.new_len
        )
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileDiff {
    /// Path on the new side (the old side for a deletion).
    pub path: BString,
    /// Source path of a rename or copy.
    pub old_path: Option<BString>,
    /// Mode of a file the diff creates: `100644`, `100755`, or `120000` for a symlink.
    pub new_file_mode: Option<String>,
    /// A changed mode (old, new), e.g. `100644` → `100755` after a chmod: with no hunks,
    /// the only change (#16).
    pub mode_change: Option<(String, String)>,
    pub binary: bool,
    pub hunks: Vec<Hunk>,
    /// Cut at [`MAX_DIFF_LINES`].
    pub truncated: bool,
}

impl FileDiff {
    pub fn line_count(&self) -> usize {
        self.hunks.iter().map(|h| h.lines.len()).sum()
    }
}

/// Which two versions of the file to compare.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffSource {
    /// Index → working tree.
    Unstaged,
    /// HEAD → index.
    Staged,
    /// A new file not yet known to git: everything is added.
    Untracked,
    /// A commit against its first parent (`None` for a root commit).
    Commit {
        parent: Option<ObjectId>,
        commit: ObjectId,
    },
}

/// The hash a repository names its objects by: SHA-1, or SHA-256 for one made with `git
/// init --object-format=sha256`. For where no object id is at hand to take it from (an
/// unborn branch); its [`gix::hash::Kind::empty_tree`] stands in for a missing parent.
pub fn object_format(
    git: &GitCli,
    cwd: &Path,
    cancel: &CancelToken,
) -> Result<gix::hash::Kind, GitError> {
    let output = git
        .command(cwd)
        .args(["rev-parse", "--show-object-format"])
        .read_only()
        .run(cancel)?;
    match output.stdout_str().trim() {
        "sha1" => Ok(gix::hash::Kind::Sha1),
        "sha256" => Ok(gix::hash::Kind::Sha256),
        other => Err(GitError::new(
            crate::cli::GitErrorKind::Other,
            "git rev-parse",
            format!("unknown object format “{other}”"),
        )),
    }
}

/// The `git diff` arguments shared by [`file_diff`] and [`whole_diff`], up to (not
/// including) the `--` that a pathspec follows.
fn diff_args(source: &DiffSource, context: u32, ignore_whitespace: bool) -> Vec<OsString> {
    let mut args: Vec<OsString> = [
        "diff",
        "--no-ext-diff",
        "--no-color",
        "--src-prefix=a/",
        "--dst-prefix=b/",
        "-M",
    ]
    .iter()
    .map(OsString::from)
    .collect();
    args.push(format!("-U{context}").into());
    if ignore_whitespace {
        args.push("-w".into());
    }
    match source {
        DiffSource::Staged => args.push("--cached".into()),
        DiffSource::Commit { parent, commit } => {
            // A root commit is compared with the empty tree of its own hash (SHA-1 or SHA-256).
            let parent = parent.unwrap_or_else(|| commit.kind().empty_tree());
            args.push(parent.to_string().into());
            args.push(commit.to_string().into());
        }
        DiffSource::Unstaged | DiffSource::Untracked => {}
    }
    args
}

/// The diff of one file. `old_path` should be given for renames so both sides are found.
pub fn file_diff(
    git: &GitCli,
    workdir: &Path,
    source: &DiffSource,
    path: &BString,
    old_path: Option<&BString>,
    options: DiffOptions,
    cancel: &CancelToken,
) -> Result<FileDiff, ReadError> {
    if *source == DiffSource::Untracked {
        return untracked_diff(workdir, path);
    }
    let mut args = diff_args(source, options.context, options.ignore_whitespace);
    args.push("--".into());
    args.extend(old_path.into_iter().chain([path]).map(|p| os(p).to_owned()));

    let output = git
        .command(workdir)
        .args(&args)
        .env("GIT_LITERAL_PATHSPECS", "1")
        .read_only()
        .run(cancel)
        .map_err(|err| explain_refused_diff(git, workdir, err, cancel))?;
    let mut files = parse_unified(&output.stdout);
    Ok(if files.is_empty() {
        FileDiff {
            path: path.clone(),
            ..FileDiff::default()
        }
    } else {
        let mut file = files.swap_remove(0);
        // A binary diff has no `---`/`+++` lines, so the parser found no path.
        if file.path.is_empty() {
            file.path = path.clone();
        }
        file
    })
}

/// Every file of the same comparison as [`file_diff`], with no pathspec and no context
/// lines (`-U0`): what the moved-line detection needs to find a block that left one file
/// and arrived in another (#69). Context lines are left out because they only serve to keep
/// runs of changes apart, and with `-U0` the hunk boundaries do that already.
///
/// An untracked file is not part of any such diff, so it gets an empty list. The result is
/// cut at [`MAX_DIFF_LINES`] like every other diff, so a move can be missed on a huge one.
pub fn whole_diff(
    git: &GitCli,
    workdir: &Path,
    source: &DiffSource,
    options: DiffOptions,
    cancel: &CancelToken,
) -> Result<Vec<FileDiff>, ReadError> {
    if *source == DiffSource::Untracked {
        return Ok(Vec::new());
    }
    let args = diff_args(source, 0, options.ignore_whitespace);
    let output = git
        .command(workdir)
        .args(&args)
        .read_only()
        .run(cancel)
        .map_err(|err| explain_refused_diff(git, workdir, err, cancel))?;
    Ok(parse_unified(&output.stdout))
}

/// Git treats a folder it refuses (another user owns it) as no repository at all, and
/// `git diff` then compares two paths (`--no-index`) and fails with that mode's usage.
/// Asks git about the repository itself then: its error says why
/// ([`crate::GitErrorKind::DubiousOwnership`]). Other errors are returned as they are.
pub(crate) fn explain_refused_diff(
    git: &GitCli,
    workdir: &Path,
    err: GitError,
    cancel: &CancelToken,
) -> GitError {
    if !err.stderr.contains("git diff --no-index") {
        return err;
    }
    match git
        .command(workdir)
        .args(["rev-parse", "--git-dir"])
        .read_only()
        .run(cancel)
    {
        Err(refused) => refused,
        Ok(_) => err,
    }
}

fn os(path: &BString) -> &OsStr {
    OsStr::from_bytes(path)
}

/// The raw bytes of both versions of a file that a diff compares.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileVersions {
    /// `None` where the file doesn't exist on that side (added, deleted).
    pub old: Option<Vec<u8>>,
    pub new: Option<Vec<u8>>,
}

/// Both versions of a file that a diff compares, to show images. A version larger than
/// `max_bytes` is an error, so a huge file is never read into memory.
pub fn file_versions(
    git: &GitCli,
    workdir: &Path,
    source: &DiffSource,
    path: &BString,
    old_path: Option<&BString>,
    max_bytes: u64,
    cancel: &CancelToken,
) -> Result<FileVersions, ReadError> {
    let old_path = old_path.unwrap_or(path);
    // `<rev>:<path>`, or `:<path>` for the index.
    let spec = |rev: &str, path: &BString| {
        let mut spec = format!("{rev}:").into_bytes();
        spec.extend_from_slice(path);
        spec
    };
    let blob = |spec: Vec<u8>| -> Result<Option<Vec<u8>>, ReadError> {
        // Exit code 128: the path doesn't exist in that version.
        let run = |kind: &str| {
            git.command(workdir)
                .args(["cat-file", kind])
                .arg(OsStr::from_bytes(&spec))
                .allow_exit_codes(&[128])
                .read_only()
                .run(cancel)
        };
        let size = run("-s")?;
        if size.status != 0 {
            return Ok(None);
        }
        if size.stdout_str().trim().parse::<u64>()? > max_bytes {
            return Err("the file is too large to show".into());
        }
        Ok(Some(run("blob")?.stdout))
    };
    let worktree = || -> Result<Option<Vec<u8>>, ReadError> {
        let full = workdir.join(os(path));
        match std::fs::metadata(&full) {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err.into()),
            Ok(meta) if meta.len() > max_bytes => Err("the file is too large to show".into()),
            Ok(_) => Ok(Some(std::fs::read(&full)?)),
        }
    };
    let (old, new) = match source {
        // The index is `:0:<path>`: a bare `:<path>` reads a file named `1:x` as stage 1 of
        // `x` (#15).
        DiffSource::Unstaged => (blob(spec(":0", old_path))?, worktree()?),
        DiffSource::Staged => (blob(spec("HEAD", old_path))?, blob(spec(":0", path))?),
        DiffSource::Untracked => (None, worktree()?),
        DiffSource::Commit { parent, commit } => {
            let old = match parent {
                Some(parent) => blob(spec(&parent.to_string(), old_path))?,
                None => None,
            };
            (old, blob(spec(&commit.to_string(), path))?)
        }
    };
    Ok(FileVersions { old, new })
}

/// A file git does not track yet, shown as one all-added hunk.
fn untracked_diff(workdir: &Path, path: &BString) -> Result<FileDiff, ReadError> {
    let mut diff = FileDiff {
        path: path.clone(),
        ..FileDiff::default()
    };
    let full = workdir.join(Path::new(os(path)));
    let metadata = std::fs::symlink_metadata(&full)?;
    let mode = if metadata.file_type().is_symlink() {
        "120000"
    } else if std::os::unix::fs::PermissionsExt::mode(&metadata.permissions()) & 0o111 != 0 {
        "100755"
    } else {
        "100644"
    };
    diff.new_file_mode = Some(mode.to_owned());
    let content: Vec<u8> = if metadata.file_type().is_symlink() {
        std::fs::read_link(&full)?.as_os_str().as_bytes().to_vec()
    } else if metadata.is_dir() {
        return Ok(diff); // e.g. an untracked nested repository
    } else if metadata.len() > MAX_UNTRACKED_BYTES {
        diff.truncated = true;
        return Ok(diff);
    } else {
        std::fs::read(&full)?
    };
    if content[..content.len().min(8000)].contains(&0) {
        diff.binary = true;
        return Ok(diff);
    }
    if content.is_empty() {
        return Ok(diff);
    }

    let ends_with_newline = content.ends_with(b"\n");
    let body = content.strip_suffix(b"\n").unwrap_or(&content);
    let mut lines: Vec<DiffLine> = body
        .split(|b| *b == b'\n')
        .take(MAX_DIFF_LINES)
        .enumerate()
        .map(|(ix, text)| DiffLine {
            kind: LineKind::Added,
            old_line: None,
            new_line: Some(ix as u32 + 1),
            text: text.into(),
            no_newline: false,
        })
        .collect();
    diff.truncated = body.split(|b| *b == b'\n').nth(MAX_DIFF_LINES).is_some();
    if !ends_with_newline
        && !diff.truncated
        && let Some(last) = lines.last_mut()
    {
        last.no_newline = true;
    }
    diff.hunks.push(Hunk {
        old_start: 0,
        old_len: 0,
        new_start: 1,
        new_len: lines.len() as u32,
        section: String::new(),
        lines,
    });
    Ok(diff)
}

/// Parses `git diff` output (one or more files).
pub fn parse_unified(stdout: &[u8]) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    // Lines still expected in the current hunk: (old side, new side).
    let mut remaining = (0u32, 0u32);
    let mut counters = (0u32, 0u32);
    let mut total_lines = 0usize;

    let body = stdout.strip_suffix(b"\n").unwrap_or(stdout);
    for line in body.split(|b| *b == b'\n') {
        let in_hunk = remaining != (0, 0);
        if in_hunk {
            let Some(file) = files.last_mut() else { break };
            let Some(hunk) = file.hunks.last_mut() else {
                break;
            };
            let (tag, text) = line
                .split_first()
                .map_or((b' ', &b""[..]), |(t, r)| (*t, r));
            let kind = match tag {
                b'+' => LineKind::Added,
                b'-' => LineKind::Removed,
                b'\\' => {
                    if let Some(last) = hunk.lines.last_mut() {
                        last.no_newline = true;
                    }
                    continue;
                }
                _ => LineKind::Context,
            };
            let (old_line, new_line) = match kind {
                LineKind::Context => {
                    remaining = (remaining.0.saturating_sub(1), remaining.1.saturating_sub(1));
                    counters = (counters.0 + 1, counters.1 + 1);
                    (Some(counters.0 - 1), Some(counters.1 - 1))
                }
                LineKind::Removed => {
                    remaining.0 = remaining.0.saturating_sub(1);
                    counters.0 += 1;
                    (Some(counters.0 - 1), None)
                }
                LineKind::Added => {
                    remaining.1 = remaining.1.saturating_sub(1);
                    counters.1 += 1;
                    (None, Some(counters.1 - 1))
                }
            };
            total_lines += 1;
            if total_lines > MAX_DIFF_LINES {
                file.truncated = true;
                continue;
            }
            hunk.lines.push(DiffLine {
                kind,
                old_line,
                new_line,
                text: text.into(),
                no_newline: false,
            });
            continue;
        }

        // A trailing "\ No newline" right after the last line of a hunk.
        if line.starts_with(b"\\") {
            if let Some(last) = files
                .last_mut()
                .and_then(|f| f.hunks.last_mut())
                .and_then(|h| h.lines.last_mut())
            {
                last.no_newline = true;
            }
            continue;
        }
        if line.starts_with(b"diff --git ") {
            files.push(FileDiff::default());
            continue;
        }
        let Some(file) = files.last_mut() else {
            continue;
        };
        if let Some(path) = line.strip_prefix(b"+++ ") {
            if let Some(path) = header_name(path, b"b/") {
                file.path = path;
            }
        } else if let Some(path) = line.strip_prefix(b"--- ") {
            // Keep the old path for deletions (the new side is /dev/null).
            if file.path.is_empty()
                && let Some(path) = header_name(path, b"a/")
            {
                file.path = path;
            }
        } else if let Some(mode) = line.strip_prefix(b"new file mode ") {
            file.new_file_mode = Some(String::from_utf8_lossy(mode).into_owned());
        } else if let Some(mode) = line.strip_prefix(b"old mode ") {
            let old = String::from_utf8_lossy(mode).into_owned();
            file.mode_change = Some((old, String::new()));
        } else if let Some(mode) = line.strip_prefix(b"new mode ") {
            if let Some((_, new)) = &mut file.mode_change {
                *new = String::from_utf8_lossy(mode).into_owned();
            }
        } else if let Some(from) = line
            .strip_prefix(b"rename from ")
            .or_else(|| line.strip_prefix(b"copy from "))
        {
            file.old_path = Some(unquote(from).into());
        } else if let Some(to) = line
            .strip_prefix(b"rename to ")
            .or_else(|| line.strip_prefix(b"copy to "))
        {
            file.path = unquote(to).into();
        } else if line.starts_with(b"Binary files ") || line.starts_with(b"GIT binary patch") {
            file.binary = true;
        } else if line.starts_with(b"@@ ")
            && let Some(hunk) = parse_hunk_header(line)
        {
            remaining = (hunk.old_len, hunk.new_len);
            counters = (hunk.old_start, hunk.new_start);
            file.hunks.push(hunk);
        }
    }
    files
}

/// The path of a `---`/`+++` line without its `a/`/`b/` prefix; `None` for `/dev/null`.
/// Git appends a TAB to paths containing spaces (not part of the path) and C-quotes
/// paths with special bytes, prefix included (`"b/tab\there"`).
fn header_name(raw: &[u8], prefix: &[u8]) -> Option<BString> {
    let raw = raw.strip_suffix(b"\t").unwrap_or(raw);
    unquote(raw).strip_prefix(prefix).map(BString::from)
}

/// Undoes git's C-style path quoting; input that isn't quoted (or is quoted in a way git
/// never writes) comes back unchanged. The strict parsing is `files.rs`'s, shared (#15).
pub(super) fn unquote(raw: &[u8]) -> Vec<u8> {
    crate::files::unquote_c_style(raw).unwrap_or_else(|| raw.to_vec())
}

/// `@@ -12,7 +12,8 @@ fn section()` (a missing `,len` means 1).
fn parse_hunk_header(line: &[u8]) -> Option<Hunk> {
    let text = line.to_str_lossy();
    let rest = text.strip_prefix("@@ -")?;
    let (ranges, section) = rest.split_once(" @@")?;
    let (old, new) = ranges.split_once(" +")?;
    let range = |spec: &str| -> Option<(u32, u32)> {
        match spec.split_once(',') {
            Some((start, len)) => Some((start.parse().ok()?, len.parse().ok()?)),
            None => Some((spec.parse().ok()?, 1)),
        }
    };
    let (old_start, old_len) = range(old)?;
    let (new_start, new_len) = range(new)?;
    Some(Hunk {
        old_start,
        old_len,
        new_start,
        new_len,
        section: section.trim().to_owned(),
        lines: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
diff --git a/src/a.rs b/src/a.rs
index 1111111..2222222 100644
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,4 +1,4 @@ fn main() {
 keep
--- this removed line starts with dashes
+++ this added line starts with pluses
 keep too
-last
\\ No newline at end of file
+last
diff --git a/old.txt b/new.txt
similarity index 90%
rename from old.txt
rename to new.txt
index 3333333..4444444 100644
--- a/old.txt
+++ b/new.txt
@@ -2 +2 @@
-x
+y
diff --git a/img.png b/img.png
index 5555555..6666666 100644
Binary files a/img.png and b/img.png differ
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
index 7777777..0000000
--- a/gone.txt
+++ /dev/null
@@ -1 +0,0 @@
-bye
";

    #[test]
    fn parses_hunks_renames_binary_and_deletions() {
        let files = parse_unified(SAMPLE.as_bytes());
        assert_eq!(files.len(), 4);

        let a = &files[0];
        assert_eq!(a.path, "src/a.rs");
        assert_eq!(a.hunks.len(), 1);
        let hunk = &a.hunks[0];
        assert_eq!(hunk.section, "fn main() {");
        let kinds: Vec<_> = hunk.lines.iter().map(|l| l.kind).collect();
        use LineKind::*;
        assert_eq!(kinds, [Context, Removed, Added, Context, Removed, Added]);
        // Lines that look like file headers are content while the hunk is open.
        assert_eq!(
            hunk.lines[1].text,
            "-- this removed line starts with dashes"
        );
        assert_eq!(hunk.lines[2].text, "++ this added line starts with pluses");
        assert!(hunk.lines[4].no_newline);
        assert!(!hunk.lines[5].no_newline);
        // Line numbers.
        assert_eq!(
            (hunk.lines[0].old_line, hunk.lines[0].new_line),
            (Some(1), Some(1))
        );
        assert_eq!(
            (hunk.lines[1].old_line, hunk.lines[1].new_line),
            (Some(2), None)
        );
        assert_eq!(
            (hunk.lines[2].old_line, hunk.lines[2].new_line),
            (None, Some(2))
        );
        assert_eq!(
            (hunk.lines[3].old_line, hunk.lines[3].new_line),
            (Some(3), Some(3))
        );

        assert_eq!(files[1].path, "new.txt");
        assert_eq!(files[1].old_path.as_ref().unwrap(), "old.txt");
        assert_eq!(
            (files[1].hunks[0].old_start, files[1].hunks[0].old_len),
            (2, 1)
        );

        assert!(files[2].binary);
        assert_eq!(files[3].path, "gone.txt");
        assert_eq!(files[3].hunks[0].lines[0].kind, LineKind::Removed);
    }

    #[test]
    fn quoted_paths_are_unquoted() {
        let text = "diff --git \"a/we\\\"ird\\tname.txt\" \"b/we\\\"ird\\tname.txt\"\n\
                    --- \"a/we\\\"ird\\tname.txt\"\n\
                    +++ \"b/we\\\"ird\\tname.txt\"\n\
                    @@ -1 +1 @@\n\
                    -x\n\
                    +y\n";
        let files = parse_unified(text.as_bytes());
        assert_eq!(files[0].path, "we\"ird\tname.txt");
        assert_eq!(files[0].hunks[0].lines.len(), 2);
        // Octal escapes (core.quotePath) and plain names.
        assert_eq!(unquote(b"\"\\303\\244.txt\""), "ä.txt".as_bytes());
        assert_eq!(unquote(b"plain.txt"), b"plain.txt");
        assert_eq!(header_name(b"/dev/null", b"b/"), None);
        assert_eq!(
            header_name(b"b/with space.txt\t", b"b/").unwrap(),
            "with space.txt"
        );
    }

    #[test]
    fn hunk_header_round_trip() {
        let hunk = parse_hunk_header(b"@@ -10,3 +11 @@").unwrap();
        assert_eq!(
            (hunk.old_start, hunk.old_len, hunk.new_start, hunk.new_len),
            (10, 3, 11, 1)
        );
        assert_eq!(hunk.header(), "@@ -10,3 +11,1 @@");
    }

    #[test]
    fn display_text_expands_tabs_and_drops_cr() {
        let line = DiffLine {
            kind: LineKind::Context,
            old_line: None,
            new_line: None,
            text: "\tx\r".into(),
            no_newline: false,
        };
        assert_eq!(line.display_text(4), "    x");
    }
}
