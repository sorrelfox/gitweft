//! Loading a repository for display: refs, history walk, lane layout and the first rows'
//! metadata. Blocking; run it on the job pool, never on the UI thread.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gitweft_git::read::{
    CommitMeta, HeadInfo, RefsModel, TipFilter, WalkOptions, WalkedCommit, commit_meta, graph_tips,
    open_repo, open_repo_isolated, read_refs_with, unmerged_tips, walk_history, walk_history_cli,
};
use gitweft_git::{CancelToken, GitCli, ObjectId, gix};
use gitweft_graph::{ColorId, Layout, LayoutBuilder, PALETTE_SIZE};

use crate::repo_settings;

/// Rows whose metadata is loaded before the first frame.
pub const FIRST_META_ROWS: usize = 200;
/// Rows per background metadata job after that.
pub const META_CHUNK_ROWS: usize = 5_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RefKind {
    /// The checked-out branch (or detached HEAD).
    Head,
    Local,
    Remote,
    Tag,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefLabel {
    pub name: String,
    pub kind: RefKind,
}

/// Everything the graph view needs except per-commit text, which loads lazily.
pub struct GraphModel {
    /// Commit id per row.
    pub commits: CommitIds,
    /// Whether the commit in each row is a merge.
    pub merges: Vec<bool>,
    pub layout: Layout,
    pub labels: HashMap<ObjectId, Vec<RefLabel>>,
    /// Every row, ordered by its commit's id, for [`Self::row_of`]'s binary search. Four
    /// bytes a commit: a map from id to row took about 55 for the ids it copied and its
    /// table, 82 MB on the Linux kernel's 1.48 million commits.
    by_id: Vec<u32>,
}

/// Commit ids by row, as their bytes one after the other: 20 bytes a SHA-1 commit, where an
/// `ObjectId` takes 33 to hold a SHA-256 one too (18 MB less on the Linux kernel).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CommitIds {
    bytes: Vec<u8>,
    /// The hash's length in bytes; every id of a repository has the same.
    width: usize,
}

impl CommitIds {
    pub fn with_capacity(rows: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(rows * gix::hash::Kind::Sha1.len_in_bytes()),
            width: gix::hash::Kind::Sha1.len_in_bytes(),
        }
    }

    pub fn push(&mut self, id: ObjectId) {
        if self.bytes.is_empty() {
            self.width = id.kind().len_in_bytes();
        }
        debug_assert_eq!(id.kind().len_in_bytes(), self.width);
        self.bytes.extend_from_slice(id.as_slice());
    }

    pub fn len(&self) -> usize {
        self.bytes.len().checked_div(self.width).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// The id in `row`.
    pub fn get(&self, row: usize) -> Option<ObjectId> {
        self.slice(row).map(ObjectId::from_bytes_or_panic)
    }

    /// The id in `row`; panics past the last row.
    pub fn id(&self, row: usize) -> ObjectId {
        self.get(row).expect("row in range")
    }

    /// The ids of `rows`.
    pub fn range(&self, rows: std::ops::Range<usize>) -> Vec<ObjectId> {
        rows.filter_map(|row| self.get(row)).collect()
    }

    pub fn iter(&self) -> impl Iterator<Item = ObjectId> + '_ {
        self.bytes
            .chunks_exact(self.width.max(1))
            .map(ObjectId::from_bytes_or_panic)
    }

    fn slice(&self, row: usize) -> Option<&[u8]> {
        let start = row.checked_mul(self.width)?;
        self.bytes.get(start..start + self.width)
    }

    fn shrink_to_fit(&mut self) {
        self.bytes.shrink_to_fit();
    }
}

impl std::ops::Index<usize> for CommitIds {
    type Output = [u8];

    /// The bytes of the id in `row`; panics past the last row.
    fn index(&self, row: usize) -> &[u8] {
        self.slice(row).expect("row in range")
    }
}

impl<const N: usize> PartialEq<[ObjectId; N]> for CommitIds {
    fn eq(&self, other: &[ObjectId; N]) -> bool {
        self.len() == N && self.iter().eq(other.iter().copied())
    }
}

impl GraphModel {
    pub fn rows(&self) -> usize {
        self.commits.len()
    }

    /// Row of a commit, if it is part of the graph.
    pub fn row_of(&self, id: &ObjectId) -> Option<usize> {
        let id = id.as_slice();
        self.by_id
            .binary_search_by(|&row| self.commits[row as usize].cmp(id))
            .ok()
            .map(|at| self.by_id[at] as usize)
    }
}

/// Where the load time went (reported by `--headless-smoke` and startup tracing).
#[derive(Debug, Clone, Copy, Default)]
pub struct LoadTimings {
    pub open: Duration,
    pub refs: Duration,
    pub walk: Duration,
    pub layout: Duration,
    pub first_meta: Duration,
    /// The history was walked by `git rev-list` (no commit-graph file) instead of gix.
    pub walk_via_git: bool,
}

pub struct LoadedRepo {
    pub name: String,
    pub repo: gix::ThreadSafeRepository,
    /// Working tree root; `None` for a bare repository.
    pub workdir: Option<std::path::PathBuf>,
    /// The git binary used for this repository's CLI operations.
    pub git: GitCli,
    pub refs: RefsModel,
    pub graph: Arc<GraphModel>,
    /// Metadata of the first rows; the rest is loaded with [`load_meta_chunk`].
    pub metas: Vec<CommitMeta>,
    pub timings: LoadTimings,
    /// How the history was walked.
    pub walk: WalkOptions,
    /// The refs left out of the walk's tips.
    pub tips: TipFilter,
    /// A shallow clone: git ignores a commit-graph file there, so none is offered.
    pub shallow: bool,
}

impl LoadedRepo {
    /// Where git runs for this repository: its working tree, or its git dir when it's bare.
    pub fn cwd(&self) -> std::path::PathBuf {
        self.workdir
            .clone()
            .unwrap_or_else(|| self.repo.git_dir().to_owned())
    }
}

/// Loads the repository containing `path`, walking its history as `walk` says (`None`:
/// as the repository's settings say, see [`repo_settings::load`]), from the tips the
/// settings' [`TipFilter`] leaves.
pub fn load_repo(
    path: &Path,
    walk: Option<WalkOptions>,
    cancel: &CancelToken,
) -> Result<LoadedRepo, String> {
    load_repo_with(path, walk, None, cancel)
}

/// In a Flatpak: the repository's credential helpers and hooks run on the host
/// (`gitweft_git::flatpak`). A failure leaves them in the sandbox, as before.
fn bridge_to_host(git: &mut GitCli, cwd: &Path, cancel: &CancelToken) {
    use gitweft_git::flatpak;
    let state = crate::paths::data_dir().join("flatpak");
    match flatpak::host_bridge(git, cwd, &state, Path::new(flatpak::SPAWN), cancel) {
        Ok(env) => {
            for (key, value) in env {
                git.set_env(key, value);
            }
        }
        Err(err) => eprintln!("gitweft: hooks and credential helpers stay in the sandbox: {err}"),
    }
}

/// What a load reads besides the repository: Gitweft's data dir (the repository's
/// settings, see [`repo_settings`]) and whether gix, which reads git config in this
/// process, may read the user's and the system's.
#[derive(Debug, Clone)]
pub struct LoadEnv {
    pub data_dir: PathBuf,
    /// The repository alone: no global or system config, no `GIT_*` environment. For
    /// tests, whose sandbox environment reaches only git children.
    pub isolated: bool,
    /// Give the repository a `gitweft.id` if it has none (the app's loads); a
    /// check like `--headless-smoke` leaves its config alone.
    pub assign_id: bool,
}

impl LoadEnv {
    /// The app's: its data dir, and the config git itself reads.
    pub fn app() -> Self {
        Self {
            data_dir: crate::paths::data_dir(),
            isolated: false,
            assign_id: false,
        }
    }

    /// A test's: its own data dir, and the repository's config only.
    pub fn isolated(data_dir: PathBuf) -> Self {
        Self {
            data_dir,
            isolated: true,
            assign_id: false,
        }
    }
}

/// [`load_repo`] with an explicit tip filter too. Each `None` is read from the
/// repository's settings (the file is read once, and only when needed).
pub fn load_repo_with(
    path: &Path,
    walk: Option<WalkOptions>,
    tips: Option<TipFilter>,
    cancel: &CancelToken,
) -> Result<LoadedRepo, String> {
    load_repo_in(path, &LoadEnv::app(), walk, tips, cancel)
}

/// [`load_repo_with`] in `env`.
pub fn load_repo_in(
    path: &Path,
    env: &LoadEnv,
    walk: Option<WalkOptions>,
    tips: Option<TipFilter>,
    cancel: &CancelToken,
) -> Result<LoadedRepo, String> {
    let mut timings = LoadTimings::default();
    let mut stopwatch = Stopwatch(Instant::now());

    let mut git = GitCli::discover().map_err(|e| e.to_string())?;
    let repo = if env.isolated {
        open_repo_isolated(path)
    } else {
        open_repo(path)
    }
    .map_err(|e| format!("{}: {e}", path.display()))?;
    if gitweft_git::flatpak::in_flatpak() {
        bridge_to_host(
            &mut git,
            repo.workdir().unwrap_or_else(|| repo.git_dir()),
            cancel,
        );
    }
    if env.assign_id {
        let root = repo.workdir().unwrap_or_else(|| repo.git_dir());
        crate::repo_id::ensure_repo_id_in(&env.data_dir, &git, root, cancel);
    }
    let (walk, filter) = match (walk, tips) {
        (Some(walk), Some(tips)) => (walk, tips),
        (walk, tips) => {
            // The same root the app keeps the settings under: the working tree, else the
            // git dir.
            let root = repo.workdir().unwrap_or_else(|| repo.git_dir());
            let settings =
                repo_settings::load_file(&repo_settings::settings_file_in(&env.data_dir, root));
            (walk.unwrap_or(settings.walk), tips.unwrap_or(settings.tips))
        }
    };
    timings.open = stopwatch.lap();
    // A reftable repository's refs come from git: gix can't read them.
    let refs = read_refs_with(&repo, &git, cancel).map_err(|e| e.to_string())?;
    timings.refs = stopwatch.lap();
    let mut tips = graph_tips(&repo, &refs, &walk, &filter);
    if walk.first_parent && !walk.current_branch {
        // Every ref gave a tip: a merged branch's own would still walk its commits. (A
        // solo ref gives one tip, which `unmerged_tips` leaves as it is.)
        let head = match &refs.head {
            HeadInfo::Branch { oid, .. } | HeadInfo::Detached { oid } => Some(*oid),
            HeadInfo::Unborn { .. } => None,
        };
        tips = unmerged_tips(&repo, &tips, head, cancel.flag()).map_err(|e| e.to_string())?;
    }
    timings.walk_via_git = !matches!(repo.commit_graph_if_enabled(), Ok(Some(_)));
    let walked = if timings.walk_via_git {
        // No commit-graph file: git's own walk is several times faster than decoding
        // every commit object here.
        walk_history_cli(&git, repo.git_dir(), &tips, &walk, cancel).map_err(|e| e.to_string())?
    } else {
        walk_history(&repo, &tips, &walk, cancel.flag()).map_err(|e| e.to_string())?
    };
    timings.walk = stopwatch.lap();
    let graph = build_graph(&refs, &walked, walk.first_parent);
    timings.layout = stopwatch.lap();
    let first = graph.rows().min(FIRST_META_ROWS);
    let metas = load_metas(&repo, &graph.commits.range(0..first));
    timings.first_meta = stopwatch.lap();

    let workdir = repo.workdir().unwrap_or_else(|| repo.git_dir());
    let name = workdir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| workdir.display().to_string());
    let workdir = repo.workdir().map(Path::to_path_buf);
    let shallow = repo.is_shallow();
    Ok(LoadedRepo {
        shallow,
        name,
        workdir,
        repo: repo.into_sync(),
        git,
        refs,
        graph: Arc::new(graph),
        metas,
        timings,
        walk,
        tips: filter,
    })
}

pub fn load_meta_chunk(repo: &gix::ThreadSafeRepository, ids: &[ObjectId]) -> Vec<CommitMeta> {
    load_metas(&repo.to_thread_local(), ids)
}

struct Stopwatch(Instant);

impl Stopwatch {
    fn lap(&mut self) -> Duration {
        let now = Instant::now();
        let elapsed = now - self.0;
        self.0 = now;
        elapsed
    }
}

fn load_metas(repo: &gix::Repository, ids: &[ObjectId]) -> Vec<CommitMeta> {
    ids.iter()
        .map(|id| {
            commit_meta(repo, *id).unwrap_or_else(|err| {
                // The row shows empty rather than the load failing; the log says why.
                eprintln!("gitweft: reading commit {id} failed: {err}");
                CommitMeta {
                    summary: Box::default(),
                    author: Arc::default(),
                    time: 0,
                }
            })
        })
        .collect()
}

/// With `first_parent`, the lanes follow first parents only: the other parents of a merge
/// were never walked, so no line may run to them (the row still counts as a merge).
fn build_graph(refs: &RefsModel, walked: &[WalkedCommit], first_parent: bool) -> GraphModel {
    // Branch tips keep a color derived from their name across reloads.
    let mut hints: HashMap<ObjectId, ColorId> = HashMap::new();
    for branch in refs.local_branches.iter().chain(&refs.remote_branches) {
        hints
            .entry(branch.target)
            .or_insert_with(|| branch_color(&branch.short_name));
    }
    // Commits whose branch color wins over a line arriving from a newer branch, so the
    // checked-out branch and the mainlines keep their colors whatever sits on top of them.
    // The checked-out branch first: its color wins over a mainline's on the same commit.
    let mut claims: HashMap<ObjectId, ColorId> = HashMap::new();
    if let HeadInfo::Branch { name, oid } = &refs.head {
        claims.insert(*oid, branch_color(name));
    }
    // A remote mainline claims only without a local branch of its name: with one, the
    // local branch is the mainline, and `origin/main` left behind inside a topic's lane
    // (or ahead of `main`) mustn't repaint that lane.
    let local: HashSet<&str> = refs
        .local_branches
        .iter()
        .map(|branch| branch.short_name.as_str())
        .collect();
    let remote_claims = refs.remote_branches.iter().filter(|branch| {
        let base = branch
            .short_name
            .split_once('/')
            .map_or("", |(_, name)| name);
        !local.contains(base)
    });
    for branch in refs.local_branches.iter().chain(remote_claims) {
        if is_mainline(&branch.short_name) {
            claims
                .entry(branch.target)
                .or_insert_with(|| branch_color(&branch.short_name));
        }
    }

    let mut builder = LayoutBuilder::with_capacity(walked.len());
    let mut commits = CommitIds::with_capacity(walked.len());
    let mut merges = Vec::with_capacity(walked.len());
    for commit in walked {
        let parents = if first_parent {
            &commit.parents[..commit.parents.len().min(1)]
        } else {
            &commit.parents[..]
        };
        match claims.get(&commit.id) {
            Some(&color) => builder.push_claiming(commit.id, parents, color),
            None => builder.push(commit.id, parents, hints.get(&commit.id).copied()),
        };
        commits.push(commit.id);
        merges.push(commit.parents.len() > 1);
    }
    commits.shrink_to_fit();

    GraphModel {
        by_id: rows_by_id(&commits),
        commits,
        merges,
        layout: builder.finish(),
        labels: ref_labels(refs),
    }
}

/// The rows of `commits` ordered by commit id. Ids are hashes, so their first eight bytes
/// order them almost always; the whole id settles the rare tie.
fn rows_by_id(commits: &CommitIds) -> Vec<u32> {
    let prefix = |row: u32| -> u64 {
        let bytes = &commits[row as usize];
        u64::from_be_bytes(bytes[..8].try_into().expect("ids are longer than 8 bytes"))
    };
    let mut rows: Vec<u32> = (0..commits.len() as u32).collect();
    rows.sort_unstable_by(|&a, &b| {
        prefix(a)
            .cmp(&prefix(b))
            .then_with(|| commits[a as usize].cmp(&commits[b as usize]))
    });
    rows
}

fn ref_labels(refs: &RefsModel) -> HashMap<ObjectId, Vec<RefLabel>> {
    let mut labels: HashMap<ObjectId, Vec<RefLabel>> = HashMap::new();
    let mut add = |id: ObjectId, name: &str, kind: RefKind| {
        labels.entry(id).or_default().push(RefLabel {
            name: name.to_owned(),
            kind,
        });
    };
    let head_branch = match &refs.head {
        HeadInfo::Branch { name, .. } => Some(name.as_str()),
        HeadInfo::Detached { oid } => {
            add(*oid, "HEAD", RefKind::Head);
            None
        }
        HeadInfo::Unborn { .. } => None,
    };
    for branch in &refs.local_branches {
        let kind = if Some(branch.short_name.as_str()) == head_branch {
            RefKind::Head
        } else {
            RefKind::Local
        };
        add(branch.target, &branch.short_name, kind);
    }
    for branch in &refs.remote_branches {
        add(branch.target, &branch.short_name, RefKind::Remote);
    }
    for tag in &refs.tags {
        add(tag.target, &tag.short_name, RefKind::Tag);
    }
    for list in labels.values_mut() {
        list.sort_by(|a, b| a.kind.cmp(&b.kind).then_with(|| a.name.cmp(&b.name)));
    }
    labels
}

/// Stable lane color for a branch name; mainlines share the first palette color, which no
/// other name gets (the hash picks among the other seven).
fn branch_color(name: &str) -> ColorId {
    if is_mainline(name) {
        return 0;
    }
    let base = name.rsplit('/').next().unwrap_or(name);
    // FNV-1a: stable across runs and platforms (unlike std's randomized hasher).
    let hash = base.bytes().fold(0x811c_9dc5u32, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193)
    });
    (1 + hash % u32::from(PALETTE_SIZE - 1)) as ColorId
}

/// `main`, `master`, `trunk` or `develop` as the last path segment (`origin/main` too).
fn is_mainline(name: &str) -> bool {
    let base = name.rsplit('/').next().unwrap_or(name);
    matches!(base, "main" | "master" | "trunk" | "develop")
}

#[cfg(test)]
mod tests {
    use super::*;
    use gitweft_git::gix::bstr::BString;
    use gitweft_git::read::RefEntry;

    fn id(n: u8) -> ObjectId {
        ObjectId::from_bytes_or_panic(&[n; 20])
    }

    /// Ids read back as pushed, SHA-1 and SHA-256 alike; rows past the end have none.
    #[test]
    fn commit_ids_read_back_by_row() {
        for width in [20, 32] {
            let ids: Vec<ObjectId> = (1..=3)
                .map(|n| ObjectId::from_bytes_or_panic(&vec![n; width]))
                .collect();
            let mut commits = CommitIds::with_capacity(ids.len());
            assert!(commits.is_empty());
            assert_eq!(commits.get(0), None);
            for &id in &ids {
                commits.push(id);
            }
            assert_eq!(commits.len(), 3);
            assert_eq!(commits.get(2), Some(ids[2]));
            assert_eq!(commits.get(3), None);
            assert_eq!(commits.range(1..3), ids[1..]);
            assert_eq!(commits.iter().collect::<Vec<_>>(), ids);
            assert_eq!(&commits[1], ids[1].as_slice());
            assert_eq!(commits, [ids[0], ids[1], ids[2]]);
        }
    }

    fn entry(prefix: &str, name: &str, target: ObjectId) -> RefEntry {
        RefEntry {
            full_name: BString::from(format!("{prefix}{name}")),
            short_name: name.to_owned(),
            target,
        }
    }

    fn walked(id: ObjectId, parents: &[ObjectId]) -> WalkedCommit {
        WalkedCommit {
            id,
            parents: parents.iter().copied().collect(),
        }
    }

    #[test]
    fn the_checked_out_branch_keeps_its_color_under_another_branch() {
        // topic (c) -> feature (b, checked out) -> main (a).
        let (a, b, c) = (id(1), id(2), id(3));
        let (feature, topic) = (branch_color("feature"), branch_color("topic"));
        assert!(
            feature != topic && feature != 0 && topic != 0,
            "distinct test colors"
        );
        let refs = RefsModel {
            head: HeadInfo::Branch {
                name: "feature".into(),
                oid: b,
            },
            local_branches: vec![
                entry("refs/heads/", "main", a),
                entry("refs/heads/", "feature", b),
                entry("refs/heads/", "topic", c),
            ],
            remote_branches: vec![],
            tags: vec![],
        };
        let graph = build_graph(
            &refs,
            &[walked(c, &[b]), walked(b, &[a]), walked(a, &[])],
            false,
        );
        let colors: Vec<ColorId> = (0..3).map(|row| graph.layout.node(row).color).collect();
        assert_eq!(colors, [topic, feature, 0]);
        assert_eq!(graph.layout.lane_count(), 1);

        // Not checked out, `feature` is just a branch under `topic`: topic's color runs on
        // down to main, which claims its own.
        let refs = RefsModel {
            head: HeadInfo::Branch {
                name: "main".into(),
                oid: a,
            },
            ..refs
        };
        let graph = build_graph(
            &refs,
            &[walked(c, &[b]), walked(b, &[a]), walked(a, &[])],
            false,
        );
        let colors: Vec<ColorId> = (0..3).map(|row| graph.layout.node(row).color).collect();
        assert_eq!(colors, [topic, topic, 0]);
    }

    /// `row_of` finds every commit by binary search over the rows ordered by id, whatever
    /// order the walk gave, and nothing else. Ids sharing their first eight bytes are
    /// told apart by the rest.
    #[test]
    fn every_commit_is_found_at_its_row() {
        let refs = RefsModel {
            head: HeadInfo::Unborn {
                name: "main".into(),
            },
            local_branches: vec![],
            remote_branches: vec![],
            tags: vec![],
        };
        // A shared prefix: the same first eight bytes, different after.
        let mut twin = [7u8; 20];
        twin[19] = 1;
        let twin = ObjectId::from_bytes_or_panic(&twin);
        let ids = [id(9), id(3), id(7), twin, id(200), id(1)];
        let walk: Vec<WalkedCommit> = ids.iter().map(|&commit| walked(commit, &[])).collect();
        let graph = build_graph(&refs, &walk, false);

        for (row, commit) in ids.iter().enumerate() {
            assert_eq!(graph.row_of(commit), Some(row), "{commit}");
        }
        assert_eq!(graph.row_of(&id(8)), None);
        assert_eq!(graph.row_of(&id(255)), None);
        assert_eq!(graph.row_of(&id(0)), None);
    }

    #[test]
    fn only_mainlines_take_the_mainline_colour() {
        for name in [
            "main",
            "master",
            "trunk",
            "develop",
            "origin/main",
            "up/develop",
        ] {
            assert_eq!(branch_color(name), 0, "{name}");
        }
        let mut seen = HashSet::new();
        for n in 0..500 {
            for name in [format!("topic/{n}"), format!("fix-{n}"), format!("w{n}")] {
                let color = branch_color(&name);
                assert!(color != 0 && color < PALETTE_SIZE, "{name}: {color}");
                seen.insert(color);
            }
        }
        // `work` and `fix` hashed to main's colour before.
        assert!(branch_color("work") != 0 && branch_color("fix") != 0);
        assert_eq!(
            seen.len(),
            usize::from(PALETTE_SIZE - 1),
            "every other colour is used"
        );
    }

    #[test]
    fn a_remote_mainline_claims_only_without_a_local_branch_of_its_name() {
        // topic (c) -> b -> a; origin/main is left at b, inside topic's lane.
        let (a, b, c) = (id(1), id(2), id(3));
        let topic = branch_color("topic");
        let walk = [walked(c, &[b]), walked(b, &[a]), walked(a, &[])];
        let refs = RefsModel {
            head: HeadInfo::Branch {
                name: "topic".into(),
                oid: c,
            },
            local_branches: vec![
                entry("refs/heads/", "main", a),
                entry("refs/heads/", "topic", c),
            ],
            remote_branches: vec![entry("refs/remotes/", "origin/main", b)],
            tags: vec![],
        };
        let colors = |refs: &RefsModel| -> Vec<ColorId> {
            let graph = build_graph(refs, &walk, false);
            (0..3).map(|row| graph.layout.node(row).color).collect()
        };
        // Local main is the mainline: origin/main doesn't repaint topic's lane at b.
        assert_eq!(colors(&refs), [topic, topic, 0]);
        // Without a local main, origin/main is the mainline and claims from b down.
        let refs = RefsModel {
            local_branches: vec![entry("refs/heads/", "topic", c)],
            ..refs
        };
        assert_eq!(colors(&refs), [topic, 0, 0]);
    }

    #[test]
    fn in_sync_remote_branches_keep_their_own_labels() {
        let (a, b, c) = (id(1), id(2), id(3));
        let refs = RefsModel {
            head: HeadInfo::Branch {
                name: "main".into(),
                oid: a,
            },
            local_branches: vec![
                entry("refs/heads/", "main", a),
                entry("refs/heads/", "feat", b),
            ],
            remote_branches: vec![
                entry("refs/remotes/", "origin/main", a),
                entry("refs/remotes/", "upstream/main", a),
                entry("refs/remotes/", "origin/feat", c),
            ],
            tags: vec![entry("refs/tags/", "v1", a)],
        };
        let labels = ref_labels(&refs);

        let names = |id: ObjectId| {
            labels[&id]
                .iter()
                .map(|l| (l.name.as_str(), l.kind))
                .collect::<Vec<_>>()
        };
        // Sorted by kind, then name.
        assert_eq!(
            names(a),
            [
                ("main", RefKind::Head),
                ("origin/main", RefKind::Remote),
                ("upstream/main", RefKind::Remote),
                ("v1", RefKind::Tag),
            ]
        );
        assert_eq!(names(b), [("feat", RefKind::Local)]);
        assert_eq!(names(c), [("origin/feat", RefKind::Remote)]);
    }
}
