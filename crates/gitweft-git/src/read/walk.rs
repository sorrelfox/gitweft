//! History traversal for the commit graph.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use gix::ObjectId;
use gix::bstr::ByteSlice;
use gix::objs::FindExt;
use gix::traverse::commit::{Parents, topo};

pub use gix::traverse::commit::ParentIds;

use super::ReadError;
use super::refs::{HeadInfo, RefsModel};
use crate::cancel::CancelToken;
use crate::cli::GitCli;

/// One commit of the walk, in graph order.
#[derive(Debug, Clone)]
pub struct WalkedCommit {
    pub id: ObjectId,
    pub parents: ParentIds,
}

/// How the graph walks the history (kept per repository).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WalkOptions {
    /// Follow only first parents (`--first-parent`): a merged branch's own commits aren't walked.
    ///
    /// With every branch as tips, git would still walk a merged branch that exists, from
    /// its own ref. A load in this mode starts only from the tips no other tip contains
    /// ([`unmerged_tips`]), so a merged branch folds into its merge commit. A branch behind
    /// another loses nothing when it is on that one's first-parent chain; HEAD's commit
    /// is always a tip.
    pub first_parent: bool,
    /// Start from HEAD alone instead of every branch, remote branch and tag.
    pub current_branch: bool,
    /// Topological order (`--topo-order`) instead of date order.
    pub topo_order: bool,
}

/// Display data of a commit, for its graph row. A graph keeps one per commit, 1.5 million on
/// the Linux kernel, so it's small: the summary is a boxed `str` (no spare capacity),
/// and the author an `Arc` a graph shares between that author's commits
/// (`GraphView::append_metas` interns them).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitMeta {
    pub summary: Box<str>,
    pub author: std::sync::Arc<Author>,
    /// Committer time, seconds since the Unix epoch.
    pub time: i64,
}

/// A commit's author as its graph row shows it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct Author {
    pub name: Box<str>,
    pub email: Box<str>,
}

/// Refs left out of the graph's tips, kept per repository.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TipFilter {
    /// Full ref names (`refs/heads/x`, `refs/remotes/origin/x`, `refs/tags/v1`) whose commits
    /// aren't tips; an entry ending in `/` leaves out every ref under it (`refs/remotes/origin/`).
    pub hidden: Vec<String>,
    /// Only this ref's commit is a tip (like `current_branch`, for another branch).
    /// Ignored when no such ref exists.
    pub solo: Option<String>,
}

impl TipFilter {
    /// Whether the ref `full_name` is hidden: an entry names it exactly, or is a prefix of
    /// it ending in `/`. Compared as bytes: a non-UTF-8 name never matches.
    pub fn hides(&self, full_name: &[u8]) -> bool {
        self.hidden.iter().any(|entry| {
            let entry = entry.as_bytes();
            entry == full_name || (entry.ends_with(b"/") && full_name.starts_with(entry))
        })
    }

    /// Nothing hidden and no solo: every ref gives a tip.
    pub fn is_empty(&self) -> bool {
        self.hidden.is_empty() && self.solo.is_none()
    }
}

/// Tips the graph starts from: HEAD plus every branch, remote branch and tag that points
/// at a commit (tags on trees or blobs are skipped). Deduplicated, in a stable order.
///
/// - An existing [`TipFilter::solo`] ref: its commit alone (`current_branch` is ignored).
/// - Else with [`WalkOptions::current_branch`]: HEAD's commit alone (none when unborn).
/// - Else HEAD's commit (always, even when its branch is hidden: the checked-out commit
///   never disappears) and every ref [`TipFilter::hides`] doesn't hide. A commit a hidden
///   ref points at is still a tip when a visible ref points there too.
pub fn graph_tips(
    repo: &gix::Repository,
    refs: &RefsModel,
    options: &WalkOptions,
    filter: &TipFilter,
) -> Vec<ObjectId> {
    let head = match &refs.head {
        HeadInfo::Branch { oid, .. } | HeadInfo::Detached { oid } => Some(*oid),
        HeadInfo::Unborn { .. } => None,
    };
    let all_refs = || {
        refs.local_branches
            .iter()
            .chain(&refs.remote_branches)
            .chain(&refs.tags)
    };
    let solo = filter.solo.as_ref().and_then(|solo| {
        all_refs()
            .find(|r| r.full_name.as_slice() == solo.as_bytes())
            .map(|r| r.target)
    });
    let mut tips: Vec<ObjectId> = if let Some(solo) = solo {
        vec![solo]
    } else if options.current_branch {
        head.into_iter().collect()
    } else {
        head.into_iter()
            .chain(
                all_refs()
                    .filter(|r| !filter.hides(&r.full_name))
                    .map(|r| r.target),
            )
            .collect()
    };
    tips.sort();
    tips.dedup();
    tips.retain(|id| {
        repo.find_header(*id)
            .is_ok_and(|header| header.kind() == gix::object::Kind::Commit)
    });
    tips
}

/// All commits reachable from `tips`, every child before its parents
/// (like `git rev-list --date-order`, or `--topo-order` and `--first-parent` as `options`
/// say). Uses the commit-graph file when available.
///
/// Each commit keeps all its parents, also when only first parents are followed (as
/// `git rev-list --first-parent --parents` prints them): a merge stays a merge.
///
/// The order is git's, with one exception: `--first-parent --topo-order` in a repository
/// without a commit-graph file. git then sorts by its older algorithm, which also waits
/// for the children behind un-walked second parents; this walk (like git with a
/// commit-graph) doesn't. Both are valid; [`walk_history_cli`] gives git's in either case.
///
/// In a shallow clone the walk ends at the boundary: those commits come out without
/// parents, as in `git rev-list`, instead of the walk failing on parents that were never
/// fetched. The commit-graph isn't used there (git ignores it in shallow repositories
/// too, and doesn't write one).
pub fn walk_history(
    repo: &gix::Repository,
    tips: &[ObjectId],
    options: &WalkOptions,
    cancel: &AtomicBool,
) -> Result<Vec<WalkedCommit>, ReadError> {
    if tips.is_empty() {
        return Ok(Vec::new());
    }
    let (objects, commit_graph) = walk_source(repo)?;
    // The commit-graph knows about how many commits there are: one allocation instead of a
    // doubling that leaves up to half of a large list unused.
    let expected = commit_graph
        .as_ref()
        .map_or(0, |graph| graph.num_commits() as usize);
    let walk = topo::Builder::from_iters(objects, tips.iter().copied(), None::<Vec<ObjectId>>)
        .sorting(if options.topo_order {
            topo::Sorting::TopoOrder
        } else {
            topo::Sorting::DateOrder
        })
        .parents(if options.first_parent {
            Parents::First
        } else {
            Parents::All
        })
        .with_commit_graph(commit_graph)
        .build()?;

    let mut commits = Vec::with_capacity(expected);
    for info in walk {
        if cancel.load(Ordering::Relaxed) {
            return Err("history walk cancelled".into());
        }
        let info = info?;
        commits.push(WalkedCommit {
            id: info.id,
            parents: info.parent_ids,
        });
    }
    // Commits newer than the commit-graph file, or a walk of fewer tips, leave the
    // capacity off; the rest goes back before the graph is built from the list.
    commits.shrink_to_fit();
    Ok(commits)
}

/// `tips` without those in another tip's history (all parents followed), in the order
/// given; `keep` (HEAD's commit), when among them, always stays. Like
/// `git merge-base --independent`, in one walk: from the parents of every tip, marking
/// the tips met on the way. Uses the commit-graph file when available; in a shallow
/// clone the walk ends at the boundary, as [`walk_history`]'s does.
///
/// With [`WalkOptions::first_parent`] the graph walks from these instead of every ref,
/// so a merged branch that still exists folds into its merge commit.
pub fn unmerged_tips(
    repo: &gix::Repository,
    tips: &[ObjectId],
    keep: Option<ObjectId>,
    cancel: &AtomicBool,
) -> Result<Vec<ObjectId>, ReadError> {
    if tips.len() < 2 {
        return Ok(tips.to_vec());
    }
    let (objects, commit_graph) = walk_source(repo)?;
    let mut buf = Vec::new();
    let mut starts = Vec::new();
    for tip in tips {
        let commit = objects.find_commit_iter(tip, &mut buf)?;
        starts.extend(commit.parent_ids());
    }
    let mut unmet: std::collections::HashSet<ObjectId> = tips.iter().copied().collect();
    let walk = gix::traverse::commit::Simple::new(starts, &objects)
        .parents(Parents::All)
        .commit_graph(commit_graph);
    for info in walk {
        if cancel.load(Ordering::Relaxed) {
            return Err("history walk cancelled".into());
        }
        unmet.remove(&info?.id);
    }
    Ok(tips
        .iter()
        .copied()
        .filter(|tip| unmet.contains(tip) || Some(*tip) == keep)
        .collect())
}

/// The objects a walk reads (through [`ShallowBoundary`]) and the commit-graph, which
/// isn't used in a shallow repository.
fn walk_source(
    repo: &gix::Repository,
) -> Result<
    (
        ShallowBoundary<gix::OdbHandle>,
        Option<gix::commitgraph::Graph>,
    ),
    ReadError,
> {
    let mut boundary = shallow_commits(repo)?;
    boundary.sort_unstable();
    let commit_graph = if boundary.is_empty() {
        repo.commit_graph_if_enabled()?
    } else {
        None
    };
    let objects = ShallowBoundary {
        objects: repo.objects.clone(),
        boundary,
    };
    Ok((objects, commit_graph))
}

/// The object database, except that commits on the shallow boundary (sorted) come back
/// without their `parent` lines, so the walk never looks for the missing parents.
struct ShallowBoundary<F> {
    objects: F,
    boundary: Vec<ObjectId>,
}

impl<F: gix::objs::Find> gix::objs::Find for ShallowBoundary<F> {
    fn try_find<'a>(
        &self,
        id: &gix::oid,
        buffer: &'a mut Vec<u8>,
    ) -> Result<Option<gix::objs::Data<'a>>, gix::objs::find::Error> {
        if self.boundary.binary_search(&id.to_owned()).is_err() {
            return self.objects.try_find(id, buffer);
        }
        let mut raw = Vec::new();
        let Some(object) = self.objects.try_find(id, &mut raw)? else {
            return Ok(None);
        };
        buffer.clear();
        if object.kind == gix::object::Kind::Commit {
            strip_parents(object.data, buffer);
        } else {
            buffer.extend_from_slice(object.data);
        }
        Ok(Some(gix::objs::Data {
            kind: object.kind,
            object_hash: object.object_hash,
            data: buffer,
        }))
    }
}

/// Copies a raw commit object without its `parent` header lines.
fn strip_parents(commit: &[u8], out: &mut Vec<u8>) {
    let header_end = commit.find(b"\n\n").map_or(commit.len(), |end| end + 1);
    let (header, message) = commit.split_at(header_end);
    for line in header.split_inclusive(|b| *b == b'\n') {
        if !line.starts_with(b"parent ") {
            out.extend_from_slice(line);
        }
    }
    out.extend_from_slice(message);
}

/// The shallow boundary: commits whose parents the repository doesn't have (the
/// `shallow` file in the common git dir, so linked worktrees share it). Empty when the
/// repository isn't shallow.
pub fn shallow_commits(repo: &gix::Repository) -> Result<Vec<ObjectId>, ReadError> {
    Ok(repo
        .shallow_commits()?
        .map(|commits| commits.iter().copied().collect())
        .unwrap_or_default())
}

pub fn commit_meta(repo: &gix::Repository, id: ObjectId) -> Result<CommitMeta, ReadError> {
    let commit = repo.find_commit(id)?;
    let summary = commit.message()?.summary().to_str_lossy().into_owned();
    let author = commit.author()?;
    Ok(CommitMeta {
        summary: summary.into_boxed_str(),
        author: std::sync::Arc::new(Author {
            name: author.name.to_str_lossy().into(),
            email: author.email.to_str_lossy().into(),
        }),
        time: commit.time()?.seconds,
    })
}

/// Same result as [`walk_history`], computed by `git rev-list`. Several times faster
/// than the gix walk when the repository has no commit-graph file, because git's
/// commit parser is highly tuned for exactly this.
pub fn walk_history_cli(
    git: &GitCli,
    git_dir: &Path,
    tips: &[ObjectId],
    options: &WalkOptions,
    cancel: &CancelToken,
) -> Result<Vec<WalkedCommit>, ReadError> {
    if tips.is_empty() {
        return Ok(Vec::new());
    }
    // GIT_DIR is resolved against the working directory, so it must be absolute.
    let git_dir = std::path::absolute(git_dir)?;
    // Tips go through stdin: repositories can have thousands of refs.
    let stdin: String = tips.iter().map(|tip| format!("{tip}\n")).collect();
    let order = if options.topo_order {
        "--topo-order"
    } else {
        "--date-order"
    };
    let first_parent = options.first_parent.then_some("--first-parent");
    let output = git
        .command(&git_dir)
        .env("GIT_DIR", &git_dir)
        .args(["rev-list", order, "--parents"])
        .args(first_parent)
        .arg("--stdin")
        .stdin(stdin)
        .read_only()
        .run(cancel)?;
    let mut commits = parse_rev_list_parents(&output.stdout)?;
    commits.shrink_to_fit();
    Ok(commits)
}

/// Parses `git rev-list --parents` output: `<commit> <parent>...` per line.
fn parse_rev_list_parents(stdout: &[u8]) -> Result<Vec<WalkedCommit>, ReadError> {
    stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| {
            let mut ids = line.split(|byte| *byte == b' ').map(ObjectId::from_hex);
            let id = ids.next().ok_or("empty rev-list line")??;
            let parents = ids.collect::<Result<ParentIds, _>>()?;
            Ok(WalkedCommit { id, parents })
        })
        .collect()
}
