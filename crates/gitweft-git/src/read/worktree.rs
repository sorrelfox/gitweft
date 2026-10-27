//! Working-tree layout queries for the file watcher: which directories hold files git
//! cares about, and which changed paths git ignores.

use std::collections::{BTreeSet, HashSet};
use std::path::Path;

use gix::bstr::{BStr, BString, ByteSlice};

use super::ReadError;
use crate::cancel::CancelToken;
use crate::cli::{GitCli, GitError};

/// Directories (repository-relative, `""` for the root) that contain tracked files,
/// according to the index. The root is always included.
pub fn tracked_dirs(repo: &gix::Repository) -> Result<BTreeSet<BString>, ReadError> {
    // A damaged index is an error, never a panic: gix-index 0.55 slices the file's
    // trailing hash without checking its length, so a file shorter than a header and a
    // hash panics it (in a release build as well: the slice is out of range there).
    let shortest = 12 + repo.object_hash().len_in_bytes() as u64;
    if let Ok(meta) = std::fs::metadata(repo.index_path())
        && meta.len() < shortest
    {
        return Err(format!("the index is damaged ({} bytes)", meta.len()).into());
    }
    // Any other panic of the decoder becomes an error too.
    let read = || repo.index_or_empty().map_err(ReadError::from);
    let index = std::panic::catch_unwind(std::panic::AssertUnwindSafe(read))
        .map_err(|_| ReadError::from("reading the index panicked"))??;
    let mut dirs = BTreeSet::from([BString::default()]);
    let mut last_parent: Option<&BStr> = None;
    for entry in index.entries() {
        let parent = parent_of(entry.path(&index));
        // Entries are sorted by path, so files of one directory are adjacent.
        if last_parent == Some(parent) {
            continue;
        }
        insert_ancestors(&mut dirs, parent);
        last_parent = Some(parent);
    }
    Ok(dirs)
}

/// Every ancestor directory of `paths` (repository-relative), plus the root `""`.
pub fn dirs_of<P: AsRef<[u8]>>(paths: impl IntoIterator<Item = P>) -> BTreeSet<BString> {
    let mut dirs = BTreeSet::from([BString::default()]);
    for path in paths {
        insert_ancestors(&mut dirs, parent_of(path.as_ref().as_bstr()));
    }
    dirs
}

/// Which of `paths` (repository-relative) git ignores: `.gitignore` files,
/// `info/exclude` and `core.excludesFile`. Tracked files are never reported.
pub fn ignored_paths(
    git: &GitCli,
    workdir: &Path,
    paths: &[BString],
    cancel: &CancelToken,
) -> Result<HashSet<BString>, GitError> {
    if paths.is_empty() {
        return Ok(HashSet::new());
    }
    let mut stdin = Vec::new();
    for path in paths {
        stdin.extend_from_slice(path);
        stdin.push(0);
    }
    let result = git
        .command(workdir)
        .args(["check-ignore", "-z", "--stdin"])
        .read_only()
        .stdin(stdin)
        .run(cancel);
    match result {
        Ok(output) => Ok(output
            .stdout
            .split(|b| *b == 0)
            .filter(|path| !path.is_empty())
            .map(BString::from)
            .collect()),
        // Exit status 1 means "none of these paths is ignored".
        Err(err) if err.exit_code == Some(1) => Ok(HashSet::new()),
        Err(err) => Err(err),
    }
}

fn parent_of(path: &BStr) -> &BStr {
    path.rfind_byte(b'/')
        .map_or(b"".as_bstr(), |slash| path[..slash].as_bstr())
}

/// Inserts `dir` and its ancestors. The set stays closed under "parent of", so the walk
/// stops at the first ancestor that is already known.
fn insert_ancestors(dirs: &mut BTreeSet<BString>, mut dir: &BStr) {
    while !dir.is_empty() && dirs.insert(dir.to_owned()) {
        dir = parent_of(dir);
    }
}
