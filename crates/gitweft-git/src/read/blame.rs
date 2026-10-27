//! Who last changed each line of a file (`git blame --incremental`), streamed while git
//! works, so the first lines show long before a deep history has been walked.
//!
//! Git reports runs of lines ("chunks") in the order it finds them, not in file order. A
//! commit's author and summary come with its first chunk only.
//!
//! Commits can be looked through ("ignored": reformatting, renames) with
//! [`BlameOptions::ignore_revs`] and the repository's `blame.ignoreRevsFile`. The
//! incremental format doesn't mark the lines whose blame passed through an ignored commit
//! (whatever `blame.markIgnoredLines` says), so while commits are ignored the same blame
//! also runs in git's default format, beside the streamed one, and its marks come last
//! ([`BlameUpdate::Marks`]). A line git can't give to an older commit stays with the
//! ignored one ([`LineMark::Unblamable`]).

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use gix::ObjectId;
use gix::bstr::BString;

use super::ReadError;
use super::details::Signature;
use super::diff::unquote;
use crate::cancel::CancelToken;
use crate::cli::GitCli;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BlameOptions {
    /// `-w`: a whitespace-only change doesn't take a line over.
    pub ignore_whitespace: bool,
    /// `-M -C`: lines moved within the file, or moved or copied from other files changed
    /// in the same commit, keep their original commit.
    pub detect_moves: bool,
    /// `--ignore-rev`: commits blame looks through, giving their lines to the commits
    /// before them. Ids that aren't commits of the repository (garbage-collected, say)
    /// are skipped, since git would refuse to run. Applies on top of the repository's
    /// `blame.ignoreRevsFile`, which git reads by itself.
    pub ignore_revs: Vec<ObjectId>,
}

/// A commit named by blame, with what the blame view shows about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlameCommit {
    /// The null id for lines not committed yet (working-tree blame).
    pub id: ObjectId,
    pub author: Signature,
    pub summary: String,
    /// The parent and path this commit's version of the file came from (`None` for the
    /// commit that added it).
    pub previous: Option<(ObjectId, BString)>,
    /// Blame stopped here (a range limit), so the lines may be older.
    pub boundary: bool,
}

/// A run of consecutive lines last changed by `commit`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlameChunk {
    pub commit: ObjectId,
    /// First line (1-based) in the blamed file and in the commit's version of it.
    pub final_line: u32,
    pub orig_line: u32,
    pub lines: u32,
    /// The file's path in that commit (differs across a rename).
    pub orig_path: BString,
}

/// How an ignored commit shaped a line's blame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineMark {
    /// The line passed through an ignored commit to an older one (git's `?`).
    Ignored,
    /// An ignored commit changed the line and no older commit has it, so it stays with
    /// the ignored commit (git's `*`).
    Unblamable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlameUpdate {
    /// A commit seen for the first time; always sent before its first chunk.
    Commit(BlameCommit),
    Chunk(BlameChunk),
    /// The lines (1-based, in the blamed file) that ignored commits shaped. Sent once,
    /// after every chunk, only while commits are ignored and some line is marked.
    Marks(Vec<(u32, LineMark)>),
}

/// Blames `path` (relative to the repository root) at `rev`, or the working-tree file when
/// `rev` is `None`; its uncommitted lines get the null id. `on_updates` receives each batch
/// as git produces it.
pub fn blame(
    git: &GitCli,
    cwd: &Path,
    rev: Option<ObjectId>,
    path: &[u8],
    options: BlameOptions,
    cancel: &CancelToken,
    mut on_updates: impl FnMut(Vec<BlameUpdate>),
) -> Result<(), ReadError> {
    // `--root`: the first commit owns its lines instead of being shown as a boundary.
    let mut args: Vec<OsString> = vec!["--root".into()];
    if options.ignore_whitespace {
        args.push("-w".into());
    }
    if options.detect_moves {
        args.extend(["-M".into(), "-C".into()]);
    }
    let ignored = commits_among(git, cwd, &options.ignore_revs, cancel)?;
    for id in &ignored {
        args.push(format!("--ignore-rev={id}").into());
    }
    // Git fails outright when a configured ignore file doesn't exist (the config names
    // `.git-blame-ignore-revs` on a branch that lacks it, say). Then drop the configured
    // list and pass back the files that do exist.
    let files = ignore_revs_files(git, cwd, cancel)?;
    if files.iter().any(|file| !file.is_file()) {
        args.push("--no-ignore-revs-file".into());
        for file in files.iter().filter(|file| file.is_file()) {
            args.push("--ignore-revs-file".into());
            args.push(file.as_os_str().to_owned());
        }
    }
    let ignoring = !ignored.is_empty() || files.iter().any(|file| file.is_file());
    if let Some(rev) = rev {
        args.push(rev.to_string().into());
    }
    args.push("--".into());
    args.push(OsStr::from_bytes(path).to_owned());

    std::thread::scope(|scope| {
        // The marked output comes only once git is done: that blame runs beside this one.
        let marks = ignoring.then(|| scope.spawn(|| line_marks(git, cwd, &args, cancel)));
        let result = stream_blame(git, cwd, &args, cancel, &mut on_updates);
        let marks = marks.and_then(|marks| marks.join().ok()).flatten();
        result?;
        if let Some(marks) = marks.filter(|marks| !marks.is_empty()) {
            on_updates(vec![BlameUpdate::Marks(marks)]);
        }
        Ok(())
    })
}

/// `git blame --incremental <args>`, each batch of updates as git produces it.
fn stream_blame(
    git: &GitCli,
    cwd: &Path,
    args: &[OsString],
    cancel: &CancelToken,
    on_updates: &mut impl FnMut(Vec<BlameUpdate>),
) -> Result<(), ReadError> {
    let mut command = git.command(cwd).args(["blame", "--incremental"]);
    for arg in args {
        command = command.arg(arg);
    }
    let command = command.read_only();

    let mut parser = BlameParser::default();
    let mut failed = None;
    command.run_streaming(cancel, |bytes| {
        if failed.is_none() {
            match parser.feed(bytes) {
                Ok(updates) if !updates.is_empty() => on_updates(updates),
                Ok(_) => {}
                Err(err) => failed = Some(err),
            }
        }
    })?;
    match failed {
        Some(err) => Err(err),
        None => parser.finish(),
    }
}

/// The `blame.ignoreRevsFile` git reads for this repository (the last one configured, the
/// value `git config --get` gives), resolved as git does: `~` expanded, a relative path
/// taken from the working tree root. `None` when unset. The file may not exist.
pub fn blame_ignore_revs_file(
    git: &GitCli,
    cwd: &Path,
    cancel: &CancelToken,
) -> Result<Option<PathBuf>, ReadError> {
    Ok(ignore_revs_files(git, cwd, cancel)?.pop())
}

/// Every configured `blame.ignoreRevsFile`, resolved as in [`blame_ignore_revs_file`].
/// Empty values are skipped, as git 2.55 does (its documentation says they reset the
/// list, but they don't).
fn ignore_revs_files(
    git: &GitCli,
    cwd: &Path,
    cancel: &CancelToken,
) -> Result<Vec<PathBuf>, ReadError> {
    // `--type=path` expands `~` and leaves out a missing `:(optional)` file. Exit code 1:
    // not set.
    let output = git
        .command(cwd)
        .args([
            "config",
            "-z",
            "--type=path",
            "--get-all",
            "blame.ignoreRevsFile",
        ])
        .read_only()
        .allow_exit_codes(&[1])
        .run(cancel)?;
    let values: Vec<&Path> = output
        .stdout
        .split(|b| *b == 0)
        .filter(|value| !value.is_empty())
        .map(|value| Path::new(OsStr::from_bytes(value)))
        .collect();
    if values.is_empty() {
        return Ok(Vec::new());
    }
    // Blame runs from the working tree root, so relative paths start there.
    let root = if values.iter().all(|value| value.is_absolute()) {
        cwd.to_path_buf()
    } else {
        working_tree_root(git, cwd, cancel)
    };
    Ok(values.into_iter().map(|value| root.join(value)).collect())
}

/// The working tree root, or `cwd` when there's none (a bare repository, where git stays
/// in the directory it runs in).
fn working_tree_root(git: &GitCli, cwd: &Path, cancel: &CancelToken) -> PathBuf {
    git.command(cwd)
        .args(["rev-parse", "--show-toplevel"])
        .read_only()
        .run(cancel)
        .ok()
        .map(|output| {
            let mut root = output.stdout;
            while root.last() == Some(&b'\n') {
                root.pop();
            }
            root
        })
        .filter(|root| !root.is_empty())
        .map(|root| PathBuf::from(OsString::from_vec(root)))
        .unwrap_or_else(|| cwd.to_path_buf())
}

/// Those of `ids` that are commits of the repository.
fn commits_among(
    git: &GitCli,
    cwd: &Path,
    ids: &[ObjectId],
    cancel: &CancelToken,
) -> Result<Vec<ObjectId>, ReadError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let input: String = ids.iter().map(|id| format!("{id}\n")).collect();
    // A missing object prints `<id> missing`.
    let output = git
        .command(cwd)
        .args(["cat-file", "--batch-check=%(objectname) %(objecttype)"])
        .stdin(input)
        .read_only()
        .run(cancel)?;
    Ok(output
        .stdout
        .split(|b| *b == b'\n')
        .filter_map(|line| line.strip_suffix(b" commit"))
        .filter_map(|hex| ObjectId::from_hex(hex).ok())
        .collect())
}

/// The lines ignored commits shaped: the same blame (`args`) in git's default format, with
/// its marks switched on. Porcelain shows them as `ignored` / `unblamable` lines only in
/// newer git (2.43 has none); the default format's `?` and `*` are there since 2.23 (#79). `None`
/// when it fails: the marks are extra, the streamed blame reports errors.
fn line_marks(
    git: &GitCli,
    cwd: &Path,
    args: &[OsString],
    cancel: &CancelToken,
) -> Option<Vec<(u32, LineMark)>> {
    let mut command = git.command(cwd).args([
        "-c",
        "blame.markIgnoredLines=true",
        "-c",
        "blame.markUnblamableLines=true",
        "blame",
        // No author and date, full ids: nothing between the marks and the id.
        "-s",
        "-l",
    ]);
    for arg in args {
        command = command.arg(arg);
    }
    let output = command.read_only().run(cancel).ok()?;
    Some(parse_marks(&output.stdout))
}

/// The marked lines of `git blame -s -l` output: one output line per line of the file, in
/// order, each starting with its marks before the commit id (`^` a boundary, `?` passed
/// through an ignored commit, `*` left with the ignored commit).
fn parse_marks(output: &[u8]) -> Vec<(u32, LineMark)> {
    let lines = output.strip_suffix(b"\n").unwrap_or(output);
    if lines.is_empty() {
        return Vec::new();
    }
    let mut marks = Vec::new();
    for (line, text) in (1..).zip(lines.split(|b| *b == b'\n')) {
        let prefix = text.iter().take_while(|b| matches!(b, b'^' | b'?' | b'*'));
        for mark in prefix {
            match mark {
                b'?' => marks.push((line, LineMark::Ignored)),
                b'*' => marks.push((line, LineMark::Unblamable)),
                _ => {}
            }
        }
    }
    marks
}

/// `ids` with their summaries, in the given order; `None` for an id that isn't a commit
/// of this repository (garbage-collected, say).
pub fn commit_summaries(
    git: &GitCli,
    cwd: &Path,
    ids: &[ObjectId],
    cancel: &CancelToken,
) -> Result<Vec<(ObjectId, Option<String>)>, ReadError> {
    let commits = commits_among(git, cwd, ids, cancel)?;
    let mut summaries = HashMap::new();
    if !commits.is_empty() {
        let input: String = commits.iter().map(|id| format!("{id}\n")).collect();
        let output = git
            .command(cwd)
            .args([
                "log",
                "--no-show-signature",
                "--no-walk=unsorted",
                "--stdin",
                "--format=%H %s",
            ])
            .stdin(input)
            .read_only()
            .run(cancel)?;
        for line in output.stdout.split(|b| *b == b'\n') {
            let Some(space) = line.iter().position(|b| *b == b' ') else {
                continue;
            };
            if let Ok(id) = ObjectId::from_hex(&line[..space]) {
                let summary = String::from_utf8_lossy(&line[space + 1..]).into_owned();
                summaries.insert(id, summary);
            }
        }
    }
    Ok(ids
        .iter()
        .map(|id| (*id, summaries.get(id).cloned()))
        .collect())
}

/// A file's content at `rev`, or the working-tree file when `None` (the text that
/// blame's line numbers refer to).
pub fn file_content(
    git: &GitCli,
    cwd: &Path,
    rev: Option<ObjectId>,
    path: &[u8],
    cancel: &CancelToken,
) -> Result<Vec<u8>, ReadError> {
    let Some(rev) = rev else {
        return Ok(std::fs::read(cwd.join(OsStr::from_bytes(path)))?);
    };
    let mut spec = format!("{rev}:").into_bytes();
    spec.extend_from_slice(path);
    let output = git
        .command(cwd)
        .args(["cat-file", "blob"])
        .arg(OsStr::from_bytes(&spec))
        .read_only()
        .run(cancel);
    match output {
        Ok(output) => Ok(output.stdout),
        // A partial clone without this version: said so, so the view offers to fetch it
        // (#87); cat-file itself only says the name isn't valid.
        Err(mut err) => {
            let files = [(rev.to_string(), path.into())];
            if crate::partial::missing_blobs(git, cwd, &files, cancel)
                .is_ok_and(|missing| !missing.is_empty())
            {
                err.kind = crate::GitErrorKind::MissingObjects;
            }
            Err(err.into())
        }
    }
}

/// Parses `git blame --incremental` output fed in pieces of any size.
#[derive(Default)]
pub struct BlameParser {
    /// The incomplete last line.
    partial: Vec<u8>,
    /// The chunk whose lines are being read.
    entry: Option<Entry>,
    /// Commits already reported.
    seen: HashSet<ObjectId>,
}

struct Entry {
    commit: BlameCommit,
    orig_line: u32,
    final_line: u32,
    lines: u32,
}

impl BlameParser {
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<BlameUpdate>, ReadError> {
        let mut data = std::mem::take(&mut self.partial);
        data.extend_from_slice(bytes);
        let mut updates = Vec::new();
        let mut rest = &data[..];
        while let Some(end) = rest.iter().position(|b| *b == b'\n') {
            self.line(&rest[..end], &mut updates)?;
            rest = &rest[end + 1..];
        }
        self.partial = rest.to_vec();
        Ok(updates)
    }

    /// Checks that the output ended between chunks.
    pub fn finish(self) -> Result<(), ReadError> {
        if self.entry.is_some() || !self.partial.is_empty() {
            return Err("`git blame` output ended in the middle of an entry".into());
        }
        Ok(())
    }

    fn line(&mut self, line: &[u8], updates: &mut Vec<BlameUpdate>) -> Result<(), ReadError> {
        let Some(entry) = self.entry.as_mut() else {
            self.entry = Some(parse_header(line)?);
            return Ok(());
        };
        let (key, value) = match line.iter().position(|b| *b == b' ') {
            Some(space) => (&line[..space], &line[space + 1..]),
            None => (line, &b""[..]),
        };
        let text = || String::from_utf8_lossy(value).into_owned();
        match key {
            b"author" => entry.commit.author.name = text(),
            b"author-mail" => {
                let mail = text();
                entry.commit.author.email = mail
                    .strip_prefix('<')
                    .and_then(|mail| mail.strip_suffix('>'))
                    .unwrap_or(&mail)
                    .to_owned();
            }
            b"author-time" => entry.commit.author.time = std::str::from_utf8(value)?.parse()?,
            b"summary" => entry.commit.summary = text(),
            b"boundary" => entry.commit.boundary = true,
            b"previous" => {
                let space = value
                    .iter()
                    .position(|b| *b == b' ')
                    .ok_or("malformed `previous` line in `git blame` output")?;
                let id = ObjectId::from_hex(&value[..space])?;
                entry.commit.previous = Some((id, BString::from(unquote(&value[space + 1..]))));
            }
            // The last line of every chunk.
            b"filename" => {
                let Entry {
                    commit,
                    orig_line,
                    final_line,
                    lines,
                } = self.entry.take().expect("checked above");
                let chunk = BlameChunk {
                    commit: commit.id,
                    final_line,
                    orig_line,
                    lines,
                    orig_path: BString::from(unquote(value)),
                };
                if self.seen.insert(commit.id) {
                    updates.push(BlameUpdate::Commit(commit));
                }
                updates.push(BlameUpdate::Chunk(chunk));
            }
            // Committer fields and time zones aren't shown.
            _ => {}
        }
        Ok(())
    }
}

/// `<commit> <line in the commit> <line in the file> <number of lines>`.
fn parse_header(line: &[u8]) -> Result<Entry, ReadError> {
    let text = std::str::from_utf8(line)?;
    let mut fields = text.split(' ');
    let id = ObjectId::from_hex(fields.next().unwrap_or_default().as_bytes())?;
    let mut number = || -> Result<u32, ReadError> {
        Ok(fields
            .next()
            .ok_or("short chunk header in `git blame` output")?
            .parse()?)
    };
    let orig_line = number()?;
    let final_line = number()?;
    let lines = number()?;
    Ok(Entry {
        commit: BlameCommit {
            id,
            author: Signature {
                name: String::new(),
                email: String::new(),
                time: 0,
            },
            summary: String::new(),
            previous: None,
            boundary: false,
        },
        orig_line,
        final_line,
        lines,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &[u8] = b"\
1111111111111111111111111111111111111111 1 1 2
author A U Thor
author-mail <a@example.invalid>
author-time 1767225600
author-tz +0000
committer A U Thor
committer-mail <a@example.invalid>
committer-time 1767225600
committer-tz +0000
summary First
boundary
filename \"we\\\"ird.txt\"
2222222222222222222222222222222222222222 3 3 1
author B
author-mail <b@example.invalid>
author-time 1767225700
author-tz +0000
committer B
committer-mail <b@example.invalid>
committer-time 1767225700
committer-tz +0000
summary Second
previous 1111111111111111111111111111111111111111 old.txt
filename new.txt
1111111111111111111111111111111111111111 2 4 1
filename old.txt
";

    fn id(digit: char) -> ObjectId {
        ObjectId::from_hex(digit.to_string().repeat(40).as_bytes()).unwrap()
    }

    #[test]
    fn parses_commits_once_and_chunks_in_order() {
        let mut parser = BlameParser::default();
        let updates = parser.feed(SAMPLE).unwrap();
        parser.finish().unwrap();

        let [
            BlameUpdate::Commit(first),
            BlameUpdate::Chunk(a),
            BlameUpdate::Commit(second),
            BlameUpdate::Chunk(b),
            BlameUpdate::Chunk(c),
        ] = updates.as_slice()
        else {
            panic!("unexpected updates: {updates:#?}");
        };
        assert_eq!(first.id, id('1'));
        assert_eq!(first.author.name, "A U Thor");
        assert_eq!(first.author.email, "a@example.invalid");
        assert_eq!(first.author.time, 1_767_225_600);
        assert_eq!(first.summary, "First");
        assert!(first.boundary);
        assert_eq!(first.previous, None);
        assert_eq!((a.final_line, a.orig_line, a.lines), (1, 1, 2));
        assert_eq!(a.orig_path, "we\"ird.txt");

        assert_eq!(second.previous, Some((id('1'), BString::from("old.txt"))));
        assert!(!second.boundary);
        assert_eq!((b.commit, b.final_line, b.lines), (id('2'), 3, 1));
        assert_eq!(b.orig_path, "new.txt");
        // A commit's later chunks carry only the file name.
        assert_eq!((c.commit, c.final_line, c.orig_line), (id('1'), 4, 2));
        assert_eq!(c.orig_path, "old.txt");
    }

    #[test]
    fn pieces_of_any_size_give_the_same_result() {
        let mut whole = BlameParser::default();
        let expected = whole.feed(SAMPLE).unwrap();
        for size in [1, 2, 7, 64] {
            let mut parser = BlameParser::default();
            let mut updates = Vec::new();
            for piece in SAMPLE.chunks(size) {
                updates.extend(parser.feed(piece).unwrap());
            }
            parser.finish().unwrap();
            assert_eq!(updates, expected, "pieces of {size} bytes");
        }
    }

    #[test]
    fn marks_belong_to_their_lines() {
        // git 2.55's `blame -s -l` with marks, one commit ignored.
        let output = b"\
^7d37f9d25622239eeed690e413f1aa1726aa8d1 1) fn foo() {
^?7d37f9d25622239eeed690e413f1aa1726aa8d 2)     a();
?7d37f9d25622239eeed690e413f1aa1726aa8d1 3)     ?b(); // a mark-like text
*355d0e892fa325fbd1ad8786da3fe4618e9ee64 4)     c();
7d37f9d25622239eeed690e413f1aa1726aa8d1f 5) }
";
        use LineMark::*;
        assert_eq!(
            parse_marks(output),
            [(2, Ignored), (3, Ignored), (4, Unblamable)]
        );
        assert!(parse_marks(b"").is_empty());
        assert_eq!(parse_marks(b"*355d0e8 1) x"), [(1, Unblamable)]);
    }

    #[test]
    fn truncated_output_is_an_error() {
        let mut parser = BlameParser::default();
        parser.feed(&SAMPLE[..SAMPLE.len() / 2]).unwrap();
        assert!(parser.finish().is_err());
    }
}
