//! In-process, read-only repository access via gix.

mod authors;
mod blame;
mod compare;
mod dangling;
mod details;
mod diff;
mod history;
mod pickaxe;
mod published;
mod range_diff;
mod reflog;
mod refs;

pub use authors::{Person, recent_authors};
pub use blame::{
    BlameChunk, BlameCommit, BlameOptions, BlameParser, BlameUpdate, LineMark, blame,
    blame_ignore_revs_file, commit_summaries, file_content,
};
pub use compare::{
    ComparedCommit, Comparison, changed_files, compare, count_commits, count_merges, is_ancestor,
    merge_base,
};
pub use dangling::{DanglingCommit, dangling_commits};
pub use history::{FileRevision, file_history};
pub use pickaxe::{Pickaxe, pickaxe};
pub use published::{Published, published_commits};
pub use range_diff::{
    HARDER_CREATION_FACTOR, InterdiffKind, InterdiffRow, RangeChange, RangePair, interdiff_rows,
    interdiff_section, parse_range_diff, range_diff,
};
pub use reflog::{ReflogEntry, reflog, unreachable_from_refs};
pub use tree::{TreeEntry, TreeEntryKind, tree_entries};

pub use diff::{
    DiffLine, DiffOptions, DiffSource, FULL_CONTEXT, FileDiff, FileVersions, Hunk, LineKind,
    MAX_DIFF_LINES, file_diff, file_versions, object_format, parse_unified, whole_diff,
};
mod status;
mod tracking;
mod tree;
mod walk;
mod worktree;

pub use tracking::{Tracking, branch_tracking};

pub use worktree::{dirs_of, ignored_paths, tracked_dirs};

pub use status::{BranchStatus, EntryKind, Status, StatusEntry, parse_porcelain_v2, read_status};

pub use details::{
    ChangeKind, CommitDetails, CommitSignature, FileChange, LineCount, Signature, SignatureStatus,
    commit_details, signature_marks,
};
pub use refs::{
    HeadInfo, RefEntry, RefsModel, read_refs, read_refs_cli, read_refs_with, uses_reftable,
};
pub use walk::{
    Author, CommitMeta, ParentIds, TipFilter, WalkOptions, WalkedCommit, commit_meta, graph_tips,
    shallow_commits, unmerged_tips, walk_history, walk_history_cli,
};

pub type ReadError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// A `limit` that lists everything: the largest count git's `--max-count` takes (a larger
/// one is "not an integer" to it).
pub const NO_LIMIT: usize = 2_147_483_647;

/// Opens the repository containing `path` (searching upwards, like `git` does).
/// The path is made absolute first, so no relative path leaks into later git commands.
pub fn open_repo(path: &std::path::Path) -> Result<gix::Repository, ReadError> {
    Ok(gix::discover(gitweft_platform::fs::canonicalize(path)?)?)
}

/// [`open_repo`] with the repository's own config only: no user, system or git-install
/// config, and no environment. For tests: gix reads config in this process, which a
/// sandbox's environment (set for git children) doesn't reach.
pub fn open_repo_isolated(path: &std::path::Path) -> Result<gix::Repository, ReadError> {
    let isolated = gix::open::Options::isolated();
    let trust = gix::sec::trust::Mapping {
        full: isolated.clone(),
        reduced: isolated,
    };
    let repo = gix::ThreadSafeRepository::discover_opts(
        gitweft_platform::fs::canonicalize(path)?,
        Default::default(),
        trust,
    )?;
    Ok(repo.to_thread_local())
}
