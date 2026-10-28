//! Files in and out of the repository: patches, archives, what `git clean` would remove,
//! and ignore rules.
//!
//! * [`format_patch`] / [`apply_mailbox`]: a commit as a mailbox patch and back
//!   (`git format-patch`, `git am --3way`). A stopped `git am` is left in progress for the
//!   operation banner ([`crate::merge::run_operation_action`] with `ApplyMailbox`).
//!   [`format_patches`] writes several, numbered; [`write_patch`], one to a named file.
//! * [`apply_patch`]: a plain diff onto the working tree and index (`git apply --3way`);
//!   [`unmerged_count`] counts the files a conflicting one left unmerged.
//! * [`archive`]: a commit's tree as a zip or tar.gz (`git archive`).
//! * [`clean_candidates`]: what `git clean -d [-x]` would remove, byte-exact.
//! * [`ignore_pattern_for_path`], [`ignore_pattern_for_extension`], [`add_ignore_rule`]:
//!   gitignore lines that match exactly one path (or an extension), and appending them.
//!
//! Paths stay bytes (`BString`, `PathBuf`); option-like names (`-x.patch`) are passed as
//! `--opt=<value>` or after `--`, so git never reads them as options.

use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::{Path, PathBuf};

use gitweft_platform::bytes::path_from_bytes;
use gix::ObjectId;
use gix::bstr::BString;

use crate::cancel::CancelToken;
use crate::cli::{GitCli, GitError, GitErrorKind};

/// Writes `commit` as a patch file into `folder` (`git format-patch -1 <commit>`); returns
/// the file written. A relative `folder` is relative to `workdir`; it is created if needed.
///
/// Merge commits are refused: format-patch skips merges, so `-1 <merge>` would quietly
/// write the newest non-merge commit behind it instead. `--no-cover-letter` and
/// `--no-base` override `format.coverLetter` / `format.useAutoBase` (a second file, or a
/// failure without an upstream).
pub fn format_patch(
    git: &GitCli,
    workdir: &Path,
    commit: &ObjectId,
    folder: &Path,
    cancel: &CancelToken,
) -> Result<PathBuf, GitError> {
    format_patches(git, workdir, std::slice::from_ref(commit), folder, cancel)
        .map(|mut files| files.remove(0))
}

/// Writes `commits` as numbered patch files into `folder` (`0001-…`, `0002-…`, in the
/// order given: oldest first, for `git am`); returns the files. Each is its own
/// `format-patch -1 --start-number=<n>`, so the commits needn't be a range (a multi-select
/// can skip some) and a root commit needs no `--root`. Merges are refused as by
/// [`format_patch`], before anything is written.
pub fn format_patches(
    git: &GitCli,
    workdir: &Path,
    commits: &[ObjectId],
    folder: &Path,
    cancel: &CancelToken,
) -> Result<Vec<PathBuf>, GitError> {
    for commit in commits {
        refuse_merge(git, workdir, commit, cancel)?;
    }
    let folder = std::path::absolute(workdir.join(folder))
        .map_err(|e| GitError::new(GitErrorKind::Other, "git format-patch", e.to_string()))?;
    commits
        .iter()
        .enumerate()
        .map(|(ix, commit)| {
            let hex = commit.to_string();
            let output = git
                .command(workdir)
                .args(["format-patch", "-1", "--no-cover-letter", "--no-base"])
                .arg(format!("--start-number={}", ix + 1))
                .arg(joined("--output-directory=", folder.as_os_str()))
                .args([hex.as_str(), "--"])
                .read_only()
                .run(cancel)?;
            // One file name per line, unquoted; the folder is absolute, so the name is too.
            let name = output.stdout.strip_suffix(b"\n").unwrap_or(&output.stdout);
            if name.is_empty() {
                return Err(GitError::new(
                    GitErrorKind::Other,
                    "git format-patch",
                    format!("fatal: git wrote no patch for {hex}"),
                ));
            }
            Ok(path_from_bytes(name).into_owned())
        })
        .collect()
}

/// Writes `commit` as a mailbox patch to `file` (`git format-patch -1 --stdout`), for a
/// save dialog that names the file itself; a relative `file` is relative to `workdir`.
/// Merges are refused, as by [`format_patch`].
pub fn write_patch(
    git: &GitCli,
    workdir: &Path,
    commit: &ObjectId,
    file: &Path,
    cancel: &CancelToken,
) -> Result<(), GitError> {
    refuse_merge(git, workdir, commit, cancel)?;
    let output = git
        .command(workdir)
        .args([
            "format-patch",
            "-1",
            "--stdout",
            "--no-cover-letter",
            "--no-base",
        ])
        .args([commit.to_string().as_str(), "--"])
        .read_only()
        .run(cancel)?;
    std::fs::write(workdir.join(file), &output.stdout)
        .map_err(|e| GitError::new(GitErrorKind::Other, "git format-patch", e.to_string()))
}

/// Fails for a merge commit: format-patch skips merges, so `-1 <merge>` would quietly write
/// the newest non-merge commit behind it instead.
fn refuse_merge(
    git: &GitCli,
    workdir: &Path,
    commit: &ObjectId,
    cancel: &CancelToken,
) -> Result<(), GitError> {
    let hex = commit.to_string();
    let parents = git
        .command(workdir)
        .args(["rev-list", "--parents", "-n", "1", &hex, "--"])
        .read_only()
        .run(cancel)?;
    let ids = parents
        .stdout
        .split(u8::is_ascii_whitespace)
        .filter(|id| !id.is_empty())
        .count();
    if ids > 2 {
        return Err(GitError::new(
            GitErrorKind::Other,
            "git format-patch",
            format!("fatal: {hex} is a merge commit; it can't be written as a patch"),
        ));
    }
    Ok(())
}

/// Applies mailbox patches as commits (`git am --3way <files>`), in order.
///
/// When `git am` stops in the middle (a conflict, or a patch without the blobs a 3-way merge
/// needs: "could not build fake ancestor"), this fails with [`GitErrorKind::Conflict`] and
/// the am stays in progress; continue, skip or abort it with
/// [`crate::merge::run_operation_action`]. A file that isn't a mailbox fails with
/// [`GitErrorKind::Other`] ("Patch format detection failed.") and leaves nothing behind.
/// Staged changes are refused up front: git would stop with "Dirty index" but still leave
/// an am in progress behind, which would look like a conflict.
pub fn apply_mailbox(
    git: &GitCli,
    workdir: &Path,
    files: &[PathBuf],
    cancel: &CancelToken,
) -> Result<(), GitError> {
    if files.is_empty() {
        return Ok(());
    }
    // Exit code 1: something is staged.
    let staged = git
        .command(workdir)
        .args(["diff", "--cached", "--quiet"])
        .allow_exit_codes(&[1])
        .read_only()
        .run(cancel)?;
    if staged.status == 1 {
        return Err(GitError::new(
            GitErrorKind::Other,
            "git am",
            "Staged changes are in the way: commit, stash or unstage them, then apply the patches.",
        ));
    }
    // `rebase-apply/applying` marks an am (as opposed to an apply-backend rebase).
    let marker = git_path(git, workdir, "rebase-apply/applying", cancel)?;
    let was_running = marker.exists();
    let result = git
        .command(workdir)
        .args(["am", "--3way", "--"])
        .args(files)
        .run(cancel);
    match result {
        Ok(_) => Ok(()),
        // Conflicts are classified from git's `CONFLICT (…)` already; a missing 3-way base
        // stops the am just the same, with nothing but "Patch failed at" on stdout.
        Err(mut err) if err.kind == GitErrorKind::Other && !was_running && marker.exists() => {
            err.kind = GitErrorKind::Conflict;
            Err(err)
        }
        Err(err) => Err(err),
    }
}

/// Applies a plain diff (`git diff` output, or a mailbox patch) to the working tree and
/// the index: `git apply --3way <file>`, after `git apply --3way --check` so a patch that
/// can't apply fails with git's reason and changes nothing.
///
/// `--check` is run with `--3way` too: without it, a patch whose context no longer
/// matches would be rejected even though the 3-way merge can apply it. The 3-way mode
/// implies `--index`, so git refuses (`<path>: does not match index`) when a file the
/// patch touches has unstaged changes. A clean apply is staged; one with conflicts leaves
/// conflict markers and unmerged index entries (no operation in progress) and fails with
/// [`GitErrorKind::Conflict`].
pub fn apply_patch(
    git: &GitCli,
    workdir: &Path,
    file: &Path,
    cancel: &CancelToken,
) -> Result<(), GitError> {
    git.command(workdir)
        .args(["apply", "--3way", "--check", "--"])
        .arg(file)
        .read_only()
        .run(cancel)?;
    match git
        .command(workdir)
        .args(["apply", "--3way", "--"])
        .arg(file)
        .run(cancel)
    {
        Ok(_) => Ok(()),
        // "Applied patch to 'a.txt' with conflicts." (exit status 1).
        Err(mut err) if err.stderr.contains("with conflicts.") => {
            err.kind = GitErrorKind::Conflict;
            Err(err)
        }
        Err(err) => Err(err),
    }
}

/// How many files have unmerged index entries (conflicts), with or without an operation in
/// progress: `git diff --name-only --diff-filter=U`.
pub fn unmerged_count(
    git: &GitCli,
    workdir: &Path,
    cancel: &CancelToken,
) -> Result<usize, GitError> {
    let output = git
        .command(workdir)
        .args(["diff", "--name-only", "--diff-filter=U", "-z"])
        .read_only()
        .run(cancel)?;
    Ok(output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .count())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveFormat {
    Zip,
    /// Compressed by git itself (`tar.tgz.command` defaults to git's built-in gzip).
    TarGz,
}

/// Writes `commit`'s tree to `file` (`git archive --format=… --output=<file> <commit>`); a
/// relative `file` is relative to `workdir`. `export-ignore` / `export-subst` attributes in
/// the commit apply, and submodules are left out (their folders are empty).
///
/// Git creates the file before it resolves the commit, so a failure (or cancellation)
/// would leave an empty or partial file: it is removed if it didn't exist before.
pub fn archive(
    git: &GitCli,
    workdir: &Path,
    commit: &ObjectId,
    format: ArchiveFormat,
    file: &Path,
    cancel: &CancelToken,
) -> Result<(), GitError> {
    let file = workdir.join(file);
    let existed = file.symlink_metadata().is_ok();
    let format = match format {
        ArchiveFormat::Zip => "--format=zip",
        ArchiveFormat::TarGz => "--format=tar.gz",
    };
    let result = git
        .command(workdir)
        .args(["archive", format])
        .arg(joined("--output=", file.as_os_str()))
        .args([commit.to_string().as_str(), "--"])
        .read_only()
        .run(cancel);
    if result.is_err() && !existed {
        let _ = std::fs::remove_file(&file);
    }
    result.map(drop)
}

/// What `git clean -d` would remove: untracked files and folders (a wholly untracked folder
/// as one entry, with a trailing `/`), plus ignored ones when `include_ignored` (`-x`).
/// Paths are relative to the workdir, in git's order.
///
/// This is `git clean -n -d [-x]` itself, parsed: its `Would remove <path>` lines, with
/// C-style quoted names (`"a\nb"`) unquoted back to their bytes (`core.quotepath` is off,
/// so other non-ASCII bytes come raw). `git ls-files --others --directory -z` would give
/// bytes directly but answers a different question: it lists nested repositories (which
/// clean skips, printing `Would skip repository`), and lists a folder as a whole even when
/// it holds ignored files, which `clean -d` without `-x` leaves (it removes the untracked
/// files inside instead), or holds nothing but ignored files, which it doesn't touch at all.
pub fn clean_candidates(
    git: &GitCli,
    workdir: &Path,
    include_ignored: bool,
    cancel: &CancelToken,
) -> Result<Vec<BString>, GitError> {
    let mut command = git.command(workdir).args(["clean", "-n", "-d"]);
    if include_ignored {
        command = command.arg("-x");
    }
    let output = command.read_only().run(cancel)?;
    parse_clean_output(&output.stdout).ok_or_else(|| {
        GitError::new(
            GitErrorKind::Other,
            "git clean -n",
            "fatal: can't read git's list of files to remove",
        )
    })
}

fn parse_clean_output(bytes: &[u8]) -> Option<Vec<BString>> {
    let mut paths = Vec::new();
    for line in bytes.split(|&b| b == b'\n') {
        // `Would skip repository <path>` (nested repositories) and anything else: not removed.
        let Some(path) = line.strip_prefix(b"Would remove ") else {
            continue;
        };
        // A name that needs quoting always contains `"`, so an unquoted one never starts with it.
        let path = if path.starts_with(b"\"") {
            unquote_c_style(path)?
        } else {
            path.to_vec()
        };
        paths.push(BString::from(path));
    }
    Some(paths)
}

/// Reverses git's `quote_c_style`: `"…"` with `\\`, `\"`, `\a\b\t\n\v\f\r` and `\ooo`.
/// `None` when `quoted` isn't quoted, or is quoted wrongly. The one unquoting in this crate.
pub(crate) fn unquote_c_style(quoted: &[u8]) -> Option<Vec<u8>> {
    let inner = quoted.strip_prefix(b"\"")?.strip_suffix(b"\"")?;
    let mut out = Vec::with_capacity(inner.len());
    let mut bytes = inner.iter().copied();
    while let Some(b) = bytes.next() {
        if b != b'\\' {
            out.push(b);
            continue;
        }
        out.push(match bytes.next()? {
            b'a' => 0x07,
            b'b' => 0x08,
            b't' => b'\t',
            b'n' => b'\n',
            b'v' => 0x0b,
            b'f' => 0x0c,
            b'r' => b'\r',
            b'"' => b'"',
            b'\\' => b'\\',
            first @ b'0'..=b'3' => {
                let mut value = first - b'0';
                for _ in 0..2 {
                    let digit = bytes.next().filter(|d| (b'0'..=b'7').contains(d))?;
                    value = value * 8 + (digit - b'0');
                }
                value
            }
            _ => return None,
        });
    }
    Some(out)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IgnoreScope {
    /// The repository's top-level `.gitignore` (shared with everyone).
    Shared,
    /// `.git/info/exclude` (only this clone).
    Local,
}

/// The pattern that ignores exactly `path` (relative, `/`-separated) from the top level:
/// anchored with a leading `/`, special characters escaped. A folder (`is_dir`, or a path
/// ending in `/`, as [`clean_candidates`] gives them) gets a trailing `/`.
///
/// Escaped: `\`, `*`, `?`, `[` (with a backslash) and trailing spaces (which git would
/// trim). A leading `#` or `!` needs no escape: the pattern starts with `/`. Two bytes
/// can't be written literally in a line-based file: a carriage return (git drops one at
/// the end of a line) becomes `[\r]`, and a newline becomes `?`, which also matches any
/// other single character there.
pub fn ignore_pattern_for_path(path: &[u8], is_dir: bool) -> BString {
    let is_dir = is_dir || path.ends_with(b"/");
    let path = trim_slashes(path);
    let mut pattern = BString::from("/");
    push_escaped(&mut pattern, path, !is_dir);
    if is_dir {
        pattern.push(b'/');
    }
    pattern
}

/// `*.ext` for the file's extension (the part after the last `.` of its name, escaped
/// like [`ignore_pattern_for_path`]), or `None` without one: no dot, a trailing dot, or
/// only a leading one (`.bashrc`). Unanchored, so it matches at any depth.
pub fn ignore_pattern_for_extension(path: &[u8]) -> Option<BString> {
    let path = trim_slashes(path);
    let name = path.rsplit(|&b| b == b'/').next().unwrap_or(path);
    let dot = name.iter().rposition(|&b| b == b'.')?;
    let extension = &name[dot + 1..];
    if dot == 0 || extension.is_empty() {
        return None;
    }
    let mut pattern = BString::from("*.");
    push_escaped(&mut pattern, extension, true);
    Some(pattern)
}

fn trim_slashes(path: &[u8]) -> &[u8] {
    let start = path.iter().position(|&b| b != b'/').unwrap_or(path.len());
    let end = path
        .iter()
        .rposition(|&b| b != b'/')
        .map_or(start, |i| i + 1);
    &path[start..end]
}

/// Appends `bytes` with gitignore's special characters escaped; `ends_line` escapes the
/// trailing spaces too (git trims unescaped ones).
fn push_escaped(out: &mut BString, bytes: &[u8], ends_line: bool) {
    let trailing_spaces = if ends_line {
        bytes.iter().rev().take_while(|&&b| b == b' ').count()
    } else {
        0
    };
    let first_trailing = bytes.len() - trailing_spaces;
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'\\' | b'*' | b'?' | b'[' => out.extend_from_slice(&[b'\\', b]),
            b' ' if i >= first_trailing => out.extend_from_slice(b"\\ "),
            b'\r' => out.extend_from_slice(b"[\r]"),
            b'\n' => out.push(b'?'),
            _ => out.push(b),
        }
    }
}

/// Appends `pattern` as its own line (adding a newline to the file's last line first if it
/// lacks one; nothing if the exact line is already there). Creates the file, and `info/`,
/// if needed. `git_dir` is where `info/exclude` lives (the common dir for a linked
/// worktree); `workdir` is the top level of the working tree.
pub fn add_ignore_rule(
    workdir: &Path,
    git_dir: &Path,
    scope: IgnoreScope,
    pattern: &[u8],
) -> std::io::Result<()> {
    if pattern.is_empty() || pattern.contains(&b'\n') {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "an ignore rule is one non-empty line",
        ));
    }
    let file = match scope {
        IgnoreScope::Shared => workdir.join(".gitignore"),
        IgnoreScope::Local => git_dir.join("info").join("exclude"),
    };
    let existing = match std::fs::read(&file) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e),
    };
    // Git reads CRLF files too, dropping the `\r`.
    let present = existing
        .split(|&b| b == b'\n')
        .any(|line| line.strip_suffix(b"\r").unwrap_or(line) == pattern);
    if present {
        return Ok(());
    }
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut bytes = Vec::with_capacity(pattern.len() + 2);
    if !existing.is_empty() && !existing.ends_with(b"\n") {
        bytes.push(b'\n');
    }
    bytes.extend_from_slice(pattern);
    bytes.push(b'\n');
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&file)?
        .write_all(&bytes)
}

/// `git rev-parse --git-path <name>`, resolved against `workdir`.
fn git_path(
    git: &GitCli,
    workdir: &Path,
    name: &str,
    cancel: &CancelToken,
) -> Result<PathBuf, GitError> {
    let output = git
        .command(workdir)
        .args(["rev-parse", "--git-path", name])
        .read_only()
        .run(cancel)?;
    let path = output.stdout.strip_suffix(b"\n").unwrap_or(&output.stdout);
    Ok(workdir.join(path_from_bytes(path)))
}

/// `--opt=` followed by a path's bytes, as one argument.
fn joined(option: &str, value: &OsStr) -> OsString {
    let mut joined = OsString::from(option);
    joined.push(value);
    joined
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(bytes: &[u8]) -> BString {
        BString::from(bytes)
    }

    #[test]
    fn clean_output_is_unquoted_and_repositories_are_skipped() {
        let output = b"Would remove empty/\n\
                       Would remove \"nl\\nx\"\n\
                       Would remove sp ace\n\
                       Would skip repository u2/n\n\
                       Would remove \"we\\\"ird\"\n\
                       Would remove \"caf\\303\\251 \\\\ \\t\"\n\
                       Would remove caf\xe9\n";
        assert_eq!(
            parse_clean_output(output).unwrap(),
            [
                b(b"empty/"),
                b(b"nl\nx"),
                b(b"sp ace"),
                b(b"we\"ird"),
                b("café \\ \t".as_bytes()),
                b(b"caf\xe9"),
            ]
        );
        assert_eq!(parse_clean_output(b"").unwrap(), Vec::<BString>::new());
        // Malformed quoting is an error, not a wrong path.
        assert!(parse_clean_output(b"Would remove \"a\\q\"\n").is_none());
        assert!(parse_clean_output(b"Would remove \"a\\3\"\n").is_none());
        assert!(parse_clean_output(b"Would remove \"open\n").is_none());
    }

    #[test]
    fn path_patterns_are_anchored_and_escaped() {
        let cases: &[(&[u8], bool, &[u8])] = &[
            (b"a.txt", false, b"/a.txt"),
            (b"a*b?.txt", false, b"/a\\*b\\?.txt"),
            (b"[x].txt", false, b"/\\[x].txt"),
            (b"#notes", false, b"/#notes"),
            (b"!bang", false, b"/!bang"),
            (b"back\\slash", false, b"/back\\\\slash"),
            (b"trailing  ", false, b"/trailing\\ \\ "),
            (b"mid space", false, b"/mid space"),
            (b"dir/sub file.txt", false, b"/dir/sub file.txt"),
            (b"dir name ", true, b"/dir name /"),
            (b"u/deep/", false, b"/u/deep/"),
            (b"cr\r", false, b"/cr[\r]"),
            (b"nl\nx", false, b"/nl?x"),
            (b"caf\xe9", false, b"/caf\xe9"),
        ];
        for &(path, is_dir, expected) in cases {
            assert_eq!(
                ignore_pattern_for_path(path, is_dir),
                b(expected),
                "{:?}",
                b(path)
            );
        }
    }

    #[test]
    fn extension_patterns() {
        let pattern = |path: &[u8]| ignore_pattern_for_extension(path);
        assert_eq!(pattern(b"dir/report.tar.gz"), Some(b(b"*.gz")));
        assert_eq!(pattern(b"a.b/c.LOG"), Some(b(b"*.LOG")));
        assert_eq!(pattern(b"x.[ab]* "), Some(b(b"*.\\[ab]\\*\\ ")));
        assert_eq!(pattern(b"caf.\xe9"), Some(b(b"*.\xe9")));
        for none in [&b"noext"[..], b".bashrc", b"dir.d/noext", b"trailing.", b""] {
            assert_eq!(pattern(none), None, "{:?}", b(none));
        }
    }
}
