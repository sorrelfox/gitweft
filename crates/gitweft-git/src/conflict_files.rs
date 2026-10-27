//! Conflicts resolved by taking one side's file whole: the ones the line-by-line tool
//! can't show (a file deleted on one side and changed on the other, a binary file changed
//! on both sides, symlinks, submodules), and any other file the user wants to settle that
//! way.
//!
//! * [`conflict_sides`] reads what each side has for a conflicted path: git's index
//!   stages 1 (base), 2 (ours) and 3 (theirs), with sizes, git's binary check, symlink
//!   targets and whether the `merge` attribute makes git merge it as binary.
//! * [`binary_merge_paths`] tells which paths git merges as binary by attribute.
//! * [`take_side`] resolves the path with one side's file (or its deletion).
//!
//! "Ours" and "theirs" are the stages as git numbers them. During a rebase that is
//! reversed from what the words suggest: stage 2 is the branch being rebased onto (with
//! the commits replayed so far) and stage 3 the commit being replayed.
//!
//! Paths stay bytes and are taken literally (`GIT_LITERAL_PATHSPECS`, or plumbing that
//! reads paths from stdin), so a file named `*.txt` is only ever that file.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;

use gix::ObjectId;
use gix::bstr::{BStr, BString};

use crate::cancel::CancelToken;
use crate::cli::{GitCli, GitError, GitErrorKind};

/// Git's rule for binary content (`buffer_is_binary`): a NUL byte in the first this many
/// bytes.
const BINARY_PROBE: usize = 8000;

/// How much of each blob's content the batch reader keeps: enough for a symlink's target
/// (Linux's `PATH_MAX`), never a whole big blob.
const HEAD_BYTES: usize = 4096;

const MODE_TYPE_MASK: u32 = 0o170000;
const MODE_REGULAR: u32 = 0o100000;
const MODE_SYMLINK: u32 = 0o120000;
const MODE_GITLINK: u32 = 0o160000;
const MODE_EXECUTABLE: u32 = 0o100755;

/// One of a conflicted file's index stages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageInfo {
    pub oid: ObjectId,
    /// The index mode: `0o100644`, `0o100755`, `0o120000` (a symlink; the blob is its
    /// target) or `0o160000` (a submodule; `oid` is a commit there).
    pub mode: u32,
    /// The blob's size in bytes (0 for a submodule).
    pub size: u64,
    /// A NUL byte in the first 8000 bytes, as git decides (never for a submodule).
    pub binary: bool,
    /// For a symlink, its target (the blob's content, at most its first 4096 bytes);
    /// `None` for every other mode.
    pub symlink_target: Option<BString>,
}

impl StageInfo {
    /// A symlink or a submodule rather than a file with content.
    pub fn is_special(&self) -> bool {
        self.mode & MODE_TYPE_MASK != MODE_REGULAR
    }

    pub fn is_symlink(&self) -> bool {
        self.mode & MODE_TYPE_MASK == MODE_SYMLINK
    }

    pub fn is_submodule(&self) -> bool {
        self.mode & MODE_TYPE_MASK == MODE_GITLINK
    }

    /// A regular file with the executable bit (`0o100755`).
    pub fn is_executable(&self) -> bool {
        self.mode == MODE_EXECUTABLE
    }
}

/// What each side has for a conflicted file (git's index stages 1 base, 2 ours, 3
/// theirs); `None` = that side has no such file (it deleted it, or never had it).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConflictSides {
    pub base: Option<StageInfo>,
    pub ours: Option<StageInfo>,
    pub theirs: Option<StageInfo>,
    /// The `merge` attribute is `binary` or unset (`-merge`, or the `binary` macro), so
    /// git merged the file with its binary driver: our version is left in the working
    /// tree without conflict markers, whatever the content.
    pub binary_by_attributes: bool,
}

impl ConflictSides {
    /// Whether the line-by-line tool can't resolve it: a side is missing, either side is
    /// binary, either side is a symlink or a submodule (conflict markers can't be
    /// written into those), or git merged it as binary because of its attributes (the
    /// working file has no markers to resolve).
    pub fn whole_file(&self) -> bool {
        if self.binary_by_attributes {
            return true;
        }
        match (&self.ours, &self.theirs) {
            (Some(ours), Some(theirs)) => [ours, theirs]
                .iter()
                .any(|side| side.binary || side.is_special()),
            _ => true,
        }
    }

    /// Whether the path has any unmerged stage at all.
    pub fn is_conflicted(&self) -> bool {
        self.base.is_some() || self.ours.is_some() || self.theirs.is_some()
    }
}

/// One line of `git ls-files -u`.
#[derive(Debug, Clone, Copy)]
struct UnmergedEntry {
    mode: u32,
    oid: ObjectId,
    stage: u8,
}

fn os(path: &BStr) -> &OsStr {
    OsStr::from_bytes(path)
}

fn parse_error(command: &str, what: impl std::fmt::Display) -> GitError {
    GitError::new(
        GitErrorKind::Other,
        command,
        format!("unexpected output: {what}"),
    )
}

/// The unmerged index entries of exactly `path` (a literal pathspec naming a directory
/// would also list the files in it; those are dropped).
fn unmerged_entries(
    git: &GitCli,
    workdir: &Path,
    path: &BStr,
    cancel: &CancelToken,
) -> Result<Vec<UnmergedEntry>, GitError> {
    const COMMAND: &str = "git ls-files -u";
    let output = git
        .command(workdir)
        .args(["ls-files", "-u", "-z", "--"])
        .arg(os(path))
        .env("GIT_LITERAL_PATHSPECS", "1")
        .read_only()
        .run(cancel)?;
    let mut entries = Vec::new();
    // `<mode> SP <oid> SP <stage> TAB <path> NUL`
    for record in output.stdout.split(|&b| b == 0).filter(|r| !r.is_empty()) {
        let tab = record
            .iter()
            .position(|&b| b == b'\t')
            .ok_or_else(|| parse_error(COMMAND, String::from_utf8_lossy(record)))?;
        if record[tab + 1..] != **path {
            continue;
        }
        let fields: Vec<&[u8]> = record[..tab].split(|&b| b == b' ').collect();
        let [mode, oid, stage] = fields[..] else {
            return Err(parse_error(COMMAND, String::from_utf8_lossy(record)));
        };
        let bad = || parse_error(COMMAND, String::from_utf8_lossy(record));
        let mode = std::str::from_utf8(mode)
            .ok()
            .and_then(|m| u32::from_str_radix(m, 8).ok())
            .ok_or_else(bad)?;
        let oid = ObjectId::from_hex(oid).map_err(|_| bad())?;
        let stage = match stage {
            b"1" => 1,
            b"2" => 2,
            b"3" => 3,
            _ => return Err(bad()),
        };
        entries.push(UnmergedEntry { mode, oid, stage });
    }
    Ok(entries)
}

/// The size of the conflict markers git writes in `path` (relative to `workdir`): its
/// `conflict-marker-size` attribute, else 7. Read from HEAD's attributes like
/// [`binary_merge_paths`] (the working tree's on git before 2.40); any failure gives 7 (#16).
pub fn conflict_marker_size(
    git: &GitCli,
    workdir: &Path,
    path: &BString,
    cancel: &CancelToken,
) -> usize {
    const DEFAULT: usize = 7;
    let check = |source: Option<&str>| {
        git.command(workdir)
            .arg("check-attr")
            .args(source)
            .args(["-z", "conflict-marker-size", "--"])
            .arg(std::ffi::OsStr::from_bytes(path))
            .read_only()
            .run(cancel)
    };
    let Ok(output) = check(Some("--source=HEAD")).or_else(|_| check(None)) else {
        return DEFAULT;
    };
    // `<path> NUL conflict-marker-size NUL <value> NUL`; the value is `unspecified` when unset.
    output
        .stdout
        .split(|&b| b == 0)
        .nth(2)
        .and_then(|value| std::str::from_utf8(value).ok()?.parse().ok())
        .filter(|&size| size > 0)
        .unwrap_or(DEFAULT)
}

/// The paths among `paths` whose `merge` attribute makes git merge them with its binary
/// driver: `merge=binary`, or unset (`-merge`, or the `binary` macro, which is
/// `-diff -merge -text`). `-text` or `-diff` alone don't change how git merges.
///
/// One `git check-attr -z --stdin merge`, the paths (relative to `workdir`, taken
/// literally) written NUL-terminated on its stdin. No paths: no git run.
pub fn binary_merge_paths(
    git: &GitCli,
    workdir: &Path,
    paths: &[BString],
    cancel: &CancelToken,
) -> Result<HashSet<BString>, GitError> {
    const COMMAND: &str = "git check-attr";
    if paths.is_empty() {
        return Ok(HashSet::new());
    }
    let mut stdin = Vec::new();
    for path in paths {
        stdin.extend_from_slice(path);
        stdin.push(0);
    }
    let check = |source: Option<&str>| {
        git.command(workdir)
            .arg("check-attr")
            .args(source)
            .args(["-z", "--stdin", "merge"])
            .stdin(stdin.clone())
            .read_only()
            .run(cancel)
    };
    // The merge took the attributes of our side, while the working tree's .gitattributes
    // may be merged already (a rule only theirs added would count): read HEAD's. Git
    // before 2.40 has no `--source` (and an unborn HEAD has nothing to read): then the
    // working tree's.
    let output = match check(Some("--source=HEAD")) {
        Err(err) if err.kind == GitErrorKind::Cancelled => return Err(err),
        Err(_) => check(None)?,
        Ok(output) => output,
    };
    // `<path> NUL <attribute> NUL <value> NUL` per path.
    let stdout = output.stdout.strip_suffix(b"\0").unwrap_or(&output.stdout);
    if stdout.is_empty() {
        return Err(parse_error(COMMAND, "no output"));
    }
    let fields: Vec<&[u8]> = stdout.split(|&b| b == 0).collect();
    let (records, rest) = fields.as_chunks::<3>();
    if !rest.is_empty() {
        return Err(parse_error(COMMAND, String::from_utf8_lossy(stdout)));
    }
    Ok(records
        .iter()
        .filter(|[_, _, value]| matches!(*value, b"unset" | b"binary"))
        .map(|[path, _, _]| BString::from(*path))
        .collect())
}

/// Reads a conflicted file's stages: `git ls-files -u` for the path, then one
/// `git cat-file --batch` for the blobs, whose output is read as it streams in, keeping
/// only the size, the binary check over the first 8000 bytes and the first 4096 bytes
/// of each (a huge blob is never held in memory), and [`binary_merge_paths`] for the
/// path. A path that isn't conflicted gives [`ConflictSides::default`].
pub fn conflict_sides(
    git: &GitCli,
    workdir: &Path,
    path: &BStr,
    cancel: &CancelToken,
) -> Result<ConflictSides, GitError> {
    let entries = unmerged_entries(git, workdir, path, cancel)?;
    if entries.is_empty() {
        return Ok(ConflictSides::default());
    }
    let mut blobs: Vec<ObjectId> = Vec::new();
    for entry in &entries {
        // A submodule's commit isn't in this repository's object database.
        if entry.mode & MODE_TYPE_MASK != MODE_GITLINK && !blobs.contains(&entry.oid) {
            blobs.push(entry.oid);
        }
    }
    let probes = probe_blobs(git, workdir, &blobs, cancel)?;

    let mut sides = ConflictSides {
        binary_by_attributes: !binary_merge_paths(git, workdir, &[path.to_owned()], cancel)?
            .is_empty(),
        ..ConflictSides::default()
    };
    for entry in entries {
        let kind = entry.mode & MODE_TYPE_MASK;
        let (size, binary, symlink_target) = if kind == MODE_GITLINK {
            (0, false, None)
        } else {
            let probe = probes.get(&entry.oid).ok_or_else(|| {
                GitError::new(
                    GitErrorKind::Other,
                    "git cat-file --batch",
                    format!("object {} is missing", entry.oid),
                )
            })?;
            let target = (kind == MODE_SYMLINK).then(|| BString::from(probe.head.as_slice()));
            (probe.size, probe.binary, target)
        };
        let info = Some(StageInfo {
            oid: entry.oid,
            mode: entry.mode,
            size,
            binary,
            symlink_target,
        });
        match entry.stage {
            1 => sides.base = info,
            2 => sides.ours = info,
            _ => sides.theirs = info,
        }
    }
    Ok(sides)
}

/// What the batch reader keeps of one blob.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Probe {
    size: u64,
    /// A NUL byte in the first [`BINARY_PROBE`] bytes.
    binary: bool,
    /// The first [`HEAD_BYTES`] bytes of the content.
    head: Vec<u8>,
}

/// Size, binary check and first bytes of each blob, from one `git cat-file --batch`.
fn probe_blobs(
    git: &GitCli,
    workdir: &Path,
    blobs: &[ObjectId],
    cancel: &CancelToken,
) -> Result<HashMap<ObjectId, Probe>, GitError> {
    if blobs.is_empty() {
        return Ok(HashMap::new());
    }
    let mut stdin = Vec::new();
    for oid in blobs {
        stdin.extend_from_slice(oid.to_string().as_bytes());
        stdin.push(b'\n');
    }
    let mut parser = BatchParser::default();
    git.command(workdir)
        .args(["cat-file", "--batch"])
        .stdin(stdin)
        .read_only()
        .run_streaming(cancel, |chunk| parser.feed(chunk))?;
    parser.finish()
}

/// Incremental reader of `git cat-file --batch` output: `<oid> <type> <size> LF
/// <content> LF` per object, or `<name> missing LF`.
#[derive(Default)]
struct BatchParser {
    /// The header line read so far.
    header: Vec<u8>,
    /// Inside an object's content: its id, bytes still to come (the closing LF
    /// included), whether a NUL was seen in the probed part, and its first bytes.
    body: Option<Body>,
    results: HashMap<ObjectId, Probe>,
    error: Option<String>,
}

struct Body {
    oid: ObjectId,
    size: u64,
    /// Content bytes read so far.
    read: u64,
    binary: bool,
    /// The first [`HEAD_BYTES`] content bytes.
    head: Vec<u8>,
}

impl BatchParser {
    fn feed(&mut self, mut chunk: &[u8]) {
        while !chunk.is_empty() && self.error.is_none() {
            if let Some(body) = &mut self.body {
                // Content plus its closing LF.
                let remaining = body.size + 1 - body.read;
                let take = usize::try_from(remaining).map_or(chunk.len(), |r| r.min(chunk.len()));
                let (part, rest) = chunk.split_at(take);
                // The content in `part`, without the closing LF.
                let content_len = usize::try_from(body.size - body.read.min(body.size))
                    .unwrap_or(usize::MAX)
                    .min(part.len());
                let content = &part[..content_len];
                let probed = usize::try_from(body.read).unwrap_or(usize::MAX);
                if probed < BINARY_PROBE {
                    let window = (BINARY_PROBE - probed).min(content.len());
                    body.binary |= content[..window].contains(&0);
                }
                if body.head.len() < HEAD_BYTES {
                    let keep = (HEAD_BYTES - body.head.len()).min(content.len());
                    body.head.extend_from_slice(&content[..keep]);
                }
                body.read += take as u64;
                chunk = rest;
                if body.read == body.size + 1 {
                    let body = self.body.take().expect("in a body");
                    self.results.insert(
                        body.oid,
                        Probe {
                            size: body.size,
                            binary: body.binary,
                            head: body.head,
                        },
                    );
                }
                continue;
            }
            match chunk.iter().position(|&b| b == b'\n') {
                None => {
                    self.header.extend_from_slice(chunk);
                    chunk = &[];
                }
                Some(end) => {
                    self.header.extend_from_slice(&chunk[..end]);
                    chunk = &chunk[end + 1..];
                    let header = std::mem::take(&mut self.header);
                    self.start(&header);
                }
            }
        }
    }

    fn start(&mut self, header: &[u8]) {
        let fields: Vec<&[u8]> = header.split(|&b| b == b' ').collect();
        match fields[..] {
            // Reported as missing by `conflict_sides`.
            [_, b"missing"] => {}
            [oid, _kind, size] => {
                let oid = ObjectId::from_hex(oid).ok();
                let size = std::str::from_utf8(size).ok().and_then(|s| s.parse().ok());
                match (oid, size) {
                    (Some(oid), Some(size)) => {
                        self.body = Some(Body {
                            oid,
                            size,
                            read: 0,
                            binary: false,
                            head: Vec::new(),
                        });
                    }
                    _ => self.error = Some(String::from_utf8_lossy(header).into_owned()),
                }
            }
            _ => self.error = Some(String::from_utf8_lossy(header).into_owned()),
        }
    }

    fn finish(self) -> Result<HashMap<ObjectId, Probe>, GitError> {
        const COMMAND: &str = "git cat-file --batch";
        if let Some(error) = self.error {
            return Err(parse_error(COMMAND, error));
        }
        if self.body.is_some() || !self.header.is_empty() {
            return Err(parse_error(COMMAND, "the output ended early"));
        }
        Ok(self.results)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Ours,
    Theirs,
}

/// Resolves a conflicted file with one side taken whole; afterwards the path has no
/// unmerged stages. A path that isn't conflicted is left alone.
///
/// When that side has the file, its stage is staged exactly as it is — the same blob and
/// mode (an executable bit, a symlink), whatever `core.fileMode`, `core.symlinks` or the
/// clean filters would make of a working-tree copy — and written to the working tree
/// (`git update-index --index-info`, then `git checkout-index -f`). When that side has
/// no file (it deleted it), the file is removed from the index and the working tree
/// (`git rm -f`, which also works when the file is already gone from disk).
///
/// "Ours" is stage 2 and "theirs" stage 3, as the line-by-line tool labels them; during a
/// rebase stage 2 is the branch being rebased onto.
pub fn take_side(
    git: &GitCli,
    workdir: &Path,
    path: &BStr,
    side: Side,
    cancel: &CancelToken,
) -> Result<(), GitError> {
    let entries = unmerged_entries(git, workdir, path, cancel)?;
    if entries.is_empty() {
        return Ok(());
    }
    let stage = match side {
        Side::Ours => 2,
        Side::Theirs => 3,
    };
    let mut nul_path = path.to_vec();
    nul_path.push(0);
    match entries.iter().find(|entry| entry.stage == stage) {
        Some(entry) => {
            // A stage-0 entry replaces all of the path's unmerged stages.
            let mut info = format!("{:o} {}\t", entry.mode, entry.oid).into_bytes();
            info.extend_from_slice(&nul_path);
            git.command(workdir)
                .args(["update-index", "-z", "--index-info"])
                .stdin(info)
                .run(cancel)?;
            // Paths from stdin are taken literally.
            git.command(workdir)
                .args(["checkout-index", "-f", "-z", "--stdin"])
                .stdin(nul_path)
                .run(cancel)?;
        }
        None => {
            git.command(workdir)
                .args(["rm", "-f", "-q", "--"])
                .arg(os(path))
                .env("GIT_LITERAL_PATHSPECS", "1")
                .run(cancel)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(n: u8) -> ObjectId {
        ObjectId::from_hex(format!("{n:040x}").as_bytes()).unwrap()
    }

    fn batch(objects: &[(ObjectId, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        for (id, content) in objects {
            out.extend_from_slice(format!("{id} blob {}\n", content.len()).as_bytes());
            out.extend_from_slice(content);
            out.push(b'\n');
        }
        out
    }

    /// Parses `bytes` fed in chunks of `size`.
    fn parse(bytes: &[u8], size: usize) -> HashMap<ObjectId, Probe> {
        let mut parser = BatchParser::default();
        for chunk in bytes.chunks(size) {
            parser.feed(chunk);
        }
        parser.finish().unwrap()
    }

    fn probe(size: u64, binary: bool, head: &[u8]) -> Probe {
        Probe {
            size,
            binary,
            head: head.to_vec(),
        }
    }

    #[test]
    fn batch_output_gives_sizes_the_binary_check_and_first_bytes_in_any_chunking() {
        let mut late = vec![b'a'; BINARY_PROBE];
        late.push(0);
        let mut edge = vec![b'a'; BINARY_PROBE - 1];
        edge.push(0);
        // Just past the kept head, with a byte that must not be kept.
        let mut long = vec![b'l'; HEAD_BYTES];
        long.push(b'X');
        let exact = vec![b'e'; HEAD_BYTES];
        let objects: [(ObjectId, &[u8]); 7] = [
            (oid(1), b"text\n"),
            (oid(2), b"\0bin"),
            (oid(3), &late),
            (oid(4), &edge),
            (oid(5), b""),
            (oid(6), &long),
            (oid(7), &exact),
        ];
        let bytes = batch(&objects);
        let a_head = vec![b'a'; HEAD_BYTES];
        for size in [1, 2, 3, 7, 4095, 4096, 4097, 8000, 8001, bytes.len()] {
            let results = parse(&bytes, size);
            assert_eq!(results[&oid(1)], probe(5, false, b"text\n"), "chunk {size}");
            assert_eq!(results[&oid(2)], probe(4, true, b"\0bin"), "chunk {size}");
            assert_eq!(
                results[&oid(3)],
                probe(8001, false, &a_head),
                "chunk {size}"
            );
            assert_eq!(results[&oid(4)], probe(8000, true, &a_head), "chunk {size}");
            assert_eq!(results[&oid(5)], probe(0, false, b""), "chunk {size}");
            let long_head = &long[..HEAD_BYTES];
            assert_eq!(
                results[&oid(6)],
                probe(4097, false, long_head),
                "chunk {size}"
            );
            assert_eq!(results[&oid(7)], probe(4096, false, &exact), "chunk {size}");
        }
    }

    #[test]
    fn truncated_or_odd_batch_output_is_an_error() {
        let mut bytes = batch(&[(oid(1), b"text\n")]);
        bytes.pop();
        let mut parser = BatchParser::default();
        parser.feed(&bytes);
        assert!(parser.finish().is_err());

        let mut parser = BatchParser::default();
        parser.feed(b"nonsense\n");
        assert!(parser.finish().is_err());

        let mut parser = BatchParser::default();
        parser.feed(format!("{} missing\n", oid(1)).as_bytes());
        assert!(parser.finish().unwrap().is_empty());
    }

    fn stage(mode: u32, binary: bool) -> StageInfo {
        StageInfo {
            oid: oid(1),
            mode,
            size: 1,
            binary,
            symlink_target: None,
        }
    }

    #[test]
    fn whole_file_when_a_side_is_missing_binary_special_or_binary_by_attributes() {
        let sides = |ours, theirs| ConflictSides {
            base: Some(stage(0o100644, false)),
            ours,
            theirs,
            binary_by_attributes: false,
        };
        let text = || Some(stage(0o100644, false));
        assert!(!sides(text(), text()).whole_file());
        assert!(!sides(text(), Some(stage(0o100755, false))).whole_file());
        assert!(sides(text(), None).whole_file());
        assert!(sides(None, text()).whole_file());
        assert!(sides(text(), Some(stage(0o100644, true))).whole_file());
        assert!(sides(Some(stage(0o120000, false)), text()).whole_file());
        assert!(sides(text(), Some(stage(0o160000, false))).whole_file());
        let by_attributes = ConflictSides {
            binary_by_attributes: true,
            ..sides(text(), text())
        };
        assert!(by_attributes.whole_file());
        assert!(!ConflictSides::default().is_conflicted());
    }

    #[test]
    fn stage_kinds_follow_the_mode() {
        let kinds = |mode| {
            let s = stage(mode, false);
            (s.is_symlink(), s.is_submodule(), s.is_executable())
        };
        assert_eq!(kinds(0o100644), (false, false, false));
        assert_eq!(kinds(0o100755), (false, false, true));
        assert_eq!(kinds(0o120000), (true, false, false));
        assert_eq!(kinds(0o160000), (false, true, false));
    }
}
