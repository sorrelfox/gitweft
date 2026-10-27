//! The files of a commit: its tree, read recursively in-process with gix (a few
//! milliseconds even for large trees, and no process to spawn).

use gix::ObjectId;
use gix::bstr::BString;

use super::ReadError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TreeEntryKind {
    Directory,
    File,
    Executable,
    Symlink,
    /// A gitlink: the commit of a submodule.
    Submodule,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    /// Full path from the repository root.
    pub path: BString,
    pub kind: TreeEntryKind,
    pub id: ObjectId,
}

/// Every entry of `commit`'s tree, directories included. The walk is breadth-first, so a
/// directory always comes before its contents.
pub fn tree_entries(repo: &gix::Repository, commit: ObjectId) -> Result<Vec<TreeEntry>, ReadError> {
    let tree = repo.find_commit(commit)?.tree()?;
    let mut recorder = gix::traverse::tree::Recorder::default();
    tree.traverse().breadthfirst(&mut recorder)?;
    Ok(recorder
        .records
        .into_iter()
        .map(|entry| TreeEntry {
            kind: kind_of(entry.mode),
            path: entry.filepath,
            id: entry.oid,
        })
        .collect())
}

fn kind_of(mode: gix::objs::tree::EntryMode) -> TreeEntryKind {
    if mode.is_tree() {
        TreeEntryKind::Directory
    } else if mode.is_commit() {
        TreeEntryKind::Submodule
    } else if mode.is_link() {
        TreeEntryKind::Symlink
    } else if mode.is_executable() {
        TreeEntryKind::Executable
    } else {
        TreeEntryKind::File
    }
}
