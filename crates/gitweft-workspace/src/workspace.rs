use std::cell::RefCell;
use std::collections::{BTreeMap, HashSet};
use std::ffi::OsString;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use crate::toast::Toast;
use crate::util::format_time;
use gitweft_core::oplog::{self, UndoHistory};
use gitweft_core::paths::trash_dir;
use gitweft_core::recent;
use gitweft_core::review::{self, ReviewSession};
use gitweft_git::activity::{ActivityEntry, ActivityLog, ActivityOutcome};
use gitweft_git::askpass::{AskpassRequest, AskpassServer, PromptKind};
use gitweft_git::bisect::{
    BisectMark, BisectProgress, BisectState, bisect_mark, bisect_reset, bisect_start, bisect_state,
};
use gitweft_git::branch::{
    create_branch, delete_branch, is_valid_branch_name, rename_branch, switch_branch,
    switch_detached, switch_to_remote_branch,
};
use gitweft_git::conflict_files;
use gitweft_git::files::{
    ArchiveFormat, IgnoreScope, add_ignore_rule, apply_mailbox, apply_patch as apply_patch_file,
    archive, clean_candidates, format_patches, ignore_pattern_for_extension,
    ignore_pattern_for_path, unmerged_count, write_patch,
};
use gitweft_git::gix::bstr::BString;
use gitweft_git::merge::{
    MergeMode, MergeOutcome, Operation, OperationAction, merge, merge_preview, merge_revision,
    operation_state, run_operation_action,
};
use gitweft_git::ops::{
    CommitOptions, DiscardBackup, apply_patch, backup_worktree_file, discard_paths, stage_paths,
    unstage_paths, write_commit_graph,
};
use gitweft_git::read::Tracking;
use gitweft_git::read::{
    DiffOptions, DiffSource, HeadInfo, RefEntry, Status, branch_tracking, commit_details,
    file_diff, read_refs_with, read_status, recent_authors,
};
use gitweft_git::read::{EntryKind, StatusEntry};
use gitweft_git::read::{Published, published_commits};
use gitweft_git::remote::{
    FetchTarget, ForceMode, PullMode, PushRequest, RemoteInfo, add_remote, fetch,
    is_valid_remote_name, pull, push, push_target, remote_names, remotes as configured_remotes,
    remove_remote, rename_remote, set_remote_push_url, set_remote_url,
};
use gitweft_git::rewrite::{self, ResetMode, recorded_reset};
use gitweft_git::setup::{clone_dir_name, clone_repository, init_repository};
use gitweft_git::stash::{
    StashEntry, StashOptions, list_stashes, stash_apply, stash_branch, stash_contents, stash_drop,
    stash_push,
};
use gitweft_git::submodules::{
    SubmoduleInfo, SubmoduleState, list_submodules_nested, sync_submodules,
    update_submodules_with_events,
};
use gitweft_git::tag::{create_tag, delete_remote_tag, delete_tag, is_valid_tag_name, push_tag};
use gitweft_git::undo::{self, CHECKED_OUT, UndoEntry, Worktree};
use gitweft_git::worktrees::{
    WorktreeInfo, WorktreeTarget, add_worktree, list_worktrees, lock_worktree, prune_worktrees,
    remove_worktree, unlock_worktree, worktree_has_changes,
};
use gitweft_git::{CancelToken, GitError, GitErrorKind, Progress, StderrEvent};
use gitweft_git::{GitCli, ObjectId};
use gitweft_ui::truncated::truncated;
use gitweft_ui::{AppIcon, LookExt, Metrics, Palette, SettingsStore, widgets};
use gpui_kit::assets::IconName;
use gpui_kit::component::Selectable;
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::dialog::{DialogAction, DialogClose, DialogFooter};
use gpui_kit::component::input::InputEvent;
use gpui_kit::component::input::{Input, InputState, Textarea, TextareaState};
use gpui_kit::component::menu::DropdownMenu as _;
use gpui_kit::component::menu::PopupMenuItem;
use gpui_kit::component::{Disableable, Icon, Sizable, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;
use std::cell::Cell;
use std::collections::HashMap;
use std::path::Path;

use gitweft_core::jobs::{Lane, RepoJobs, WorkerPool};
use gitweft_core::watch::{RepoWatcher, Update};

use crate::APP_NAME;
use crate::blame_view::{BlameEvent, BlameView};
use crate::compare_view::{CompareEvent, CompareView, ExportTarget, ReviewDiff};
use crate::conflict_view::{ConflictEvent, ConflictView};
use crate::dangling_view::DanglingView;
use crate::details_view::{DetailsEvent, DetailsState, DetailsView, LoadedCommit, StashDetails};
use crate::diff_decor::decorate;
use crate::diff_view::{DiffEvent, DiffLayout, DiffView, PatchAction};
use crate::file_text::{FileText, file_text};
use crate::file_view::{FileEvent, FileView};
use crate::graph_view::{GraphEvent, GraphView, RowCommit, WipSummary};
use crate::history_view::{HistoryEvent, HistoryView};
use crate::keymap;
use crate::rebase_view::{RebaseEvent, RebaseView};
use crate::reflog_view::{Reflog, ReflogEvent, ReflogView};
use crate::staging_view::{IgnoreChoice, StagingEvent, StagingView};
use crate::tab_notes::note;
use crate::trash_view::TrashView;
use crate::tree_view::{TreeEvent, TreeModel, TreeView};
use gitweft_core::repo::{
    LoadEnv, LoadedRepo, META_CHUNK_ROWS, RefLabel, load_meta_chunk, load_repo_in,
};
use gitweft_git::lfs::{lfs_fetch, lfs_fetch_all, lfs_pull};
use gitweft_git::read::{TipFilter, WalkOptions};
use gitweft_git::rebase::{RebaseOutcome, RebasePlan, rebase_interactive};
use gitweft_git::remote::{Deepen, deepen};

/// Calls `$apply!(self, …)` with the views shown in the centre instead of the graph (the
/// conflict tool aside), topmost first, each with the method that closes it. The one list
/// of them: a new centre view goes here, and closing, covering and focus follow.
macro_rules! centre_views {
    ($apply:ident!($this:expr)) => {
        $apply!(
            $this;
            rebase_view close_rebase,
            blame_view close_blame,
            diff_view close_diff,
            file_view close_file,
            history_view close_history,
            tree_view close_tree,
            reflog_view close_reflog,
            dangling_view close_dangling,
            trash_view close_trash,
            compare_view close_compare
        )
    };
}

mod bisect_run;
mod branch_ops;
mod centre;
mod commands;
mod compare_refs;
mod conflict_header;
mod conflict_panel;
mod dialogs;
mod external;
mod file_ops;
mod graph_actions;
mod keyboard;
mod lfs_ops;
mod look;
mod panels;
mod recovery;
mod remote_ops;
mod repair;
mod review_notes;
mod rewrite_ops;
mod row_actions;
mod sidebar;
mod start;
mod stash_tag_ops;
mod status_bar;
mod submodule_ops;
mod sync;
mod toolbar;
mod undo_ops;
mod worktree_ops;

actions!(
    workspace,
    [
        Fetch,
        Pull,
        PullMerge,
        PullRebase,
        PullFastForward,
        Push,
        ForcePush,
        CancelRemote,
        UndoOperation,
        RedoOperation,
        OpenInTerminal,
        OpenInFileManager,
        ApplyPatches,
        CleanUntracked,
        CleanIgnored,
        DownloadLfsFiles,
        FetchLfsObjects,
        FetchAllLfsObjects,
        PruneLfsObjects,
        DeepenHistory,
        FetchAllHistory,
        ToggleBackgroundFetch,
        FetchEvery5,
        FetchEvery10,
        FetchEvery15,
        FetchEvery30,
        FetchEvery60,
        FocusNextPanel,
        FocusPreviousPanel,
        ShowDanglingCommits,
        ShowTrash,
        AddSubmodule,
        ShowLfsLocks,
        Commit,
        StageAll,
        UnstageAll,
        StashChanges,
        PopStash,
        NewBranch,
        NewTag,
        AddRemote,
        AddWorktree
    ]
);

pub(crate) const KEY_CONTEXT: &str = "Workspace";

/// A text field in the tab, for the keys that work while one has the focus (Commit).
pub(crate) const INPUT_KEY_CONTEXT: &str = "Workspace > Input";

/// Undo steps kept per repository (the oplog keeps them across sessions).
const UNDO_LIMIT: usize = oplog::LIMIT;

pub(crate) fn bind_keys(cx: &mut App) {
    // Text fields bind these too; theirs win while one has focus.
    let context = Some(KEY_CONTEXT);
    cx.bind_keys([
        KeyBinding::new("secondary-z", UndoOperation, context),
        KeyBinding::new("secondary-shift-z", RedoOperation, context),
        // The sidebar, the centre, the right panel.
        KeyBinding::new("f6", FocusNextPanel, context),
        // Commit from anywhere in the tab. In a text field too: the kit's own Ctrl+Enter
        // puts a newline in a multi-line one (the description) and stops there, and this
        // binding, registered after the kit's, comes first.
        KeyBinding::new("secondary-enter", Commit, context),
        KeyBinding::new("secondary-enter", Commit, Some(INPUT_KEY_CONTEXT)),
        KeyBinding::new("shift-f6", FocusPreviousPanel, context),
    ]);
    // Redo's second key on Linux and Windows; a Mac has only Cmd+Shift+Z.
    if !cfg!(target_os = "macos") {
        cx.bind_keys([KeyBinding::new("ctrl-y", RedoOperation, context)]);
    }
}

/// What a write records for undo: the refs it may move, and how undoing it treats the
/// files (see `gitweft_git::undo`).
struct Record {
    label: String,
    refs: Vec<String>,
    worktree: Worktree,
    /// The one ref is a local branch the write creates or deletes: keep its config too.
    branch_config: bool,
}

impl Record {
    /// A branch or tag (full name); its undo never touches the files.
    fn new(label: String, full_name: String) -> Self {
        Self {
            label,
            refs: vec![full_name],
            worktree: Worktree::Follow,
            branch_config: false,
        }
    }

    /// A local branch (full name) created or deleted: its undo also puts back (or
    /// removes) its `branch.<name>.*` config, such as its upstream.
    fn branch(label: String, full_name: String) -> Self {
        Self {
            branch_config: true,
            ..Self::new(label, full_name)
        }
    }

    /// Whatever is checked out when the write starts.
    fn checked_out(label: String, worktree: Worktree) -> Self {
        Self {
            label,
            refs: vec![CHECKED_OUT.to_owned()],
            worktree,
            branch_config: false,
        }
    }
}

/// Whether `path` is a folder inside the working tree at `root` that belongs to no
/// repository of its own: no folder on the way down, `path` included, has a `.git` (a
/// submodule, a linked worktree or another repository). Both are compared canonicalized.
fn in_worktree(root: &Path, path: &Path) -> bool {
    let canonical =
        |path: &Path| gitweft_platform::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
    let (root, path) = (canonical(root), canonical(path));
    let Ok(inside) = path.strip_prefix(&root) else {
        return false;
    };
    let mut dir = root.clone();
    inside.components().all(|part| {
        dir.push(part);
        std::fs::symlink_metadata(dir.join(".git")).is_err()
    })
}

/// A change to the saved undo log, run in the write lane (`oplog::update`).
type UndoChange = Box<dyn FnOnce(&mut UndoHistory) + Send>;

/// Work queued to run after a switch to a branch has succeeded.
type AfterSwitch = Box<dyn FnOnce(&mut Workspace, &mut Window, &mut Context<Workspace>)>;

/// Runs `op`, and with a `record`, returns the undo entry for what it moved.
fn recorded<T>(
    git: &GitCli,
    cwd: &Path,
    record: Option<Record>,
    cancel: &CancelToken,
    op: impl FnOnce() -> Result<T, GitError>,
) -> Result<(T, Option<UndoEntry>), GitError> {
    let Some(record) = record else {
        return op().map(|value| (value, None));
    };
    if record.branch_config
        && let [branch] = &record.refs[..]
    {
        let (value, changes, config) = undo::record_branch(git, cwd, branch, cancel, op)?;
        let entry = UndoEntry::new(record.label, changes, record.worktree).map(|mut entry| {
            entry.branch_config.extend(config);
            entry
        });
        return Ok((value, entry));
    }
    let refs: Vec<&str> = record.refs.iter().map(String::as_str).collect();
    let (value, changes) = undo::record(git, cwd, &refs, cancel, op)?;
    Ok((
        value,
        UndoEntry::new(record.label, changes, record.worktree),
    ))
}

/// A network operation, as started from the toolbar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoteOpKind {
    Fetch,
    /// Fetch one remote (named in `Workspace::fetch_one`).
    FetchOne,
    Pull(PullMode),
    Push(ForceMode),
    /// `git lfs pull`: download the LFS files of the checked-out commit.
    LfsPull,
    /// `git lfs fetch`: download them into `.git/lfs` without touching the files.
    LfsFetch,
    /// `git lfs fetch --all`: the LFS objects of every commit any ref reaches.
    LfsFetchAll,
    /// Fetch more of a shallow clone's history.
    Deepen(Deepen),
    /// `git submodule update --init --recursive` of the submodules named in
    /// `Workspace::submodule_update` (it may clone and fetch).
    SubmoduleUpdate,
}

impl RemoteOpKind {
    fn running_label(self) -> String {
        match self {
            RemoteOpKind::Fetch | RemoteOpKind::FetchOne => t!("remote.fetching"),
            RemoteOpKind::Pull(_) => t!("remote.pulling"),
            RemoteOpKind::Push(_) => t!("remote.pushing"),
            RemoteOpKind::LfsPull => t!("lfs.pulling"),
            RemoteOpKind::LfsFetch => t!("lfs.fetching"),
            RemoteOpKind::LfsFetchAll => t!("lfs.fetching_all"),
            RemoteOpKind::Deepen(_) => t!("shallow.deepening"),
            RemoteOpKind::SubmoduleUpdate => t!("submodule.updating"),
        }
        .into_owned()
    }

    fn failed_title(self) -> String {
        match self {
            RemoteOpKind::Fetch | RemoteOpKind::FetchOne => t!("remote.fetch_failed"),
            RemoteOpKind::Pull(_) => t!("remote.pull_failed"),
            RemoteOpKind::Push(_) => t!("remote.push_failed"),
            RemoteOpKind::LfsPull | RemoteOpKind::LfsFetch | RemoteOpKind::LfsFetchAll => {
                t!("lfs.failed")
            }
            RemoteOpKind::Deepen(_) => t!("shallow.failed"),
            RemoteOpKind::SubmoduleUpdate => t!("submodule.failed"),
        }
        .into_owned()
    }
}

/// The network operation in progress. One at a time: they share the write lane anyway.
struct RemoteOp {
    kind: RemoteOpKind,
    progress: Option<Progress>,
    cancel: CancelToken,
}

/// A clone in progress (start screen).
struct CloneOp {
    url: String,
    progress: Option<Progress>,
    cancel: CancelToken,
}

/// A git handle for work outside any repository (init, clone), wired like a
/// repository's: credential prompts and the activity log.
fn standalone_git(
    env: Vec<(OsString, OsString)>,
    activity: ActivityLog,
) -> Result<GitCli, GitError> {
    let mut git = GitCli::discover()?;
    for (key, value) in env {
        git.set_env(key, value);
    }
    // In a Flatpak the credential helpers are the host's (`gitweft_git::flatpak`).
    if gitweft_git::flatpak::in_flatpak() {
        let state = gitweft_core::paths::data_dir().join("flatpak");
        let spawn = std::path::Path::new(gitweft_git::flatpak::SPAWN);
        match gitweft_git::flatpak::credential_env(&git, &state, spawn) {
            Ok(bridge) => {
                for (key, value) in bridge {
                    git.set_env(key, value);
                }
            }
            Err(err) => eprintln!("gitweft: credential helpers stay in the sandbox: {err}"),
        }
    }
    git.set_activity_log(activity);
    Ok(git)
}

/// A branch operation, started from a menu or a dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
enum BranchOp {
    Create {
        name: String,
        /// Revision to start at; HEAD when `None`.
        start: Option<String>,
        switch: bool,
    },
    Rename {
        from: String,
        to: String,
    },
    Delete {
        name: String,
        force: bool,
    },
    Switch {
        name: String,
    },
    /// A remote-tracking branch: switch to (or create) the local branch of that name.
    SwitchRemote {
        remote: String,
        branch: String,
    },
    Detach {
        commit: gitweft_git::ObjectId,
    },
}

/// Work that needs the window, queued from places that don't have it (entity events);
/// it runs on the next frame.
type WindowTask = Box<dyn FnOnce(&mut Workspace, &mut Window, &mut Context<Workspace>)>;

/// What a text dialog asks for.
struct TextDialog {
    title: String,
    subtitle: Option<String>,
    placeholder: String,
    initial: String,
    /// A second, optional input (an annotated tag's message): its placeholder and initial
    /// text.
    detail: Option<(String, String)>,
    /// A checkbox under the inputs: its label and state.
    checkbox: Option<(String, Rc<Cell<bool>>)>,
    ok_label: String,
    /// OK also works with an empty first input (a stash message is optional).
    allow_empty: bool,
    /// The first input takes several lines (a review comment): Enter still confirms, and
    /// Shift+Enter starts a new line.
    multiline: bool,
}

/// What a text dialog does with (first input, second input) on OK or Enter.
type SubmitText = Rc<dyn Fn(&mut Workspace, String, String, &mut Window, &mut Context<Workspace>)>;

/// A way out offered by a dialog button.
type ChoiceAction = Rc<dyn Fn(&mut Workspace, &mut Window, &mut Context<Workspace>)>;

/// What a workspace asks of the window around it.
pub enum WorkspaceEvent {
    /// Open this repository (a worktree) in a tab of its own.
    OpenInNewTab(PathBuf),
}

impl EventEmitter<WorkspaceEvent> for Workspace {}

enum RepoState {
    Empty,
    Loading(PathBuf),
    Loaded(Box<LoadedRepo>),
    Failed(String),
}

/// Everything needed to (re)load the diff shown in the centre panel.
#[derive(Clone)]
struct DiffParams {
    title: String,
    subtitle: String,
    source: DiffSource,
    path: BString,
    old_path: Option<BString>,
    options: DiffOptions,
}

/// Repositories at least this long without a commit-graph file get the offer to write one.
const COMMIT_GRAPH_HINT_ROWS: usize = 5_000;
/// Commits scanned for co-author suggestions.
const CO_AUTHOR_HISTORY: usize = 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommitGraphBanner {
    Hidden,
    Offer,
    Writing,
}

/// One tab's content: the start screen, or a repository with its toolbar and banners.
pub struct Workspace {
    state: RepoState,
    /// Path the current repository was opened from (used to reload it).
    current_path: Option<PathBuf>,
    /// How the graph walks the history: `None` until the first load has read it from the
    /// repository's settings, then what the view menu chose.
    walk: Option<WalkOptions>,
    /// The refs hidden from the graph, or the one soloed: `None` until the first load has
    /// read them from the repository's settings.
    tips: Option<TipFilter>,
    /// What to do once git has switched to this branch (a branch dropped on another one
    /// merges after switching there).
    after_switch: Option<(String, AfterSwitch)>,
    commit_graph_banner: CommitGraphBanner,
    /// The graph's search box; kept across repositories.
    search: Entity<InputState>,
    graph_view: Option<Entity<GraphView>>,
    /// The graph rows' signature marks, kept across reloads of this repository and handed
    /// to each new graph view.
    signatures: crate::graph_view::SignatureCache,
    /// Commits whose marks are to be read next, and whether a read is running.
    signature_queue: Vec<ObjectId>,
    signatures_reading: bool,
    details_view: Entity<DetailsView>,
    _details_events: Subscription,
    _graph_events: Option<Subscription>,
    /// A file diff shown in the centre instead of the graph.
    diff_view: Option<Entity<DiffView>>,
    /// What the open diff shows (to reload it after staging parts of it).
    diff_params: Option<DiffParams>,
    _diff_events: Option<Subscription>,
    /// Diff preferences kept while the app runs (context size is per file).
    diff_layout: DiffLayout,
    diff_ignore_whitespace: bool,
    /// Whether the open views' syntax colors were computed for the dark palette; a
    /// light/dark switch highlights them again (`look::settings_changed`).
    highlight_dark: bool,
    /// The tab width the open views' lines were expanded with.
    highlight_tab: usize,
    /// Filters the sidebar's branches and tags by name.
    sidebar_filter: Entity<InputState>,
    _sidebar_filter_events: Subscription,
    /// Remotes whose branches the sidebar hides (not saved: all are open on every start).
    collapsed_remotes: HashSet<String>,
    /// The side panels' widths while a divider is dragged.
    panels: panels::Panels,
    /// Latest working-tree status of the open repository.
    status: Option<Status>,
    /// The right panel shows the working changes (not a commit).
    showing_wip: bool,
    staging_view: Entity<StagingView>,
    _staging_events: Subscription,
    /// Set on open: the first status result selects "Working changes" if dirty.
    select_wip_when_dirty: bool,
    /// The tab is shown in an active window (see [`Self::set_visible`]).
    visible: bool,
    jobs: RepoJobs,
    /// Watches the open repository; dropping it stops watching.
    watcher: Option<RepoWatcher>,
    _watch_task: Option<Task<()>>,
    /// The OS watch limit was hit: show the banner explaining the slower refresh.
    watch_limited: bool,
    /// The watcher stopped (it kept failing): the banner says so; the tab is polled.
    watch_stopped: bool,
    /// Git refuses the repository's folder (another user owns it): the folder to trust
    /// (`repair.rs`). Shown as a banner, or on the start screen when opening failed.
    untrusted: Option<PathBuf>,
    /// A merge, rebase, cherry-pick or revert in progress (the operation banner).
    operation: Option<Operation>,
    /// The conflict tool, shown in the centre instead of the graph (or a diff).
    conflict_view: Option<Entity<ConflictView>>,
    _conflict_events: Option<Subscription>,
    /// The right panel's "Conflicted files" mode (`conflict_panel.rs`).
    conflict_panel: conflict_panel::ConflictPanel,
    /// A file's blame, shown in the centre instead of the graph (or a diff).
    blame_view: Option<Entity<BlameView>>,
    _blame_events: Option<Subscription>,
    /// Cancels the job streaming into `blame_view`.
    blame_cancel: Option<CancelToken>,
    /// The blames "Blame the parent" left, newest last: the file, the revision and the
    /// first line shown, for Back.
    blame_back: Vec<(BString, Option<ObjectId>, usize)>,
    /// A file's history, shown in the centre instead of the graph; diffs and blames
    /// opened from it show on top of it.
    history_view: Option<Entity<HistoryView>>,
    _history_events: Option<Subscription>,
    history_cancel: Option<CancelToken>,
    /// A commit's files, shown in the centre instead of the graph; everything opened from
    /// it shows on top of it.
    tree_view: Option<Entity<TreeView>>,
    _tree_events: Option<Subscription>,
    /// A file as of a commit (opened from the tree), shown on top of the tree.
    file_view: Option<Entity<FileView>>,
    _file_events: Option<Subscription>,
    /// A ref's reflog, shown in the centre instead of the graph.
    reflog_view: Option<Entity<ReflogView>>,
    _reflog_events: Option<Subscription>,
    /// The ref whose reflog is shown, to re-read it when the refs change.
    reflog_ref: Option<String>,
    /// The dangling commits (`git fsck`), shown in the centre; the search's stop.
    dangling_view: Option<Entity<DanglingView>>,
    _dangling_events: Option<Subscription>,
    dangling_cancel: Option<CancelToken>,
    /// The Trash page (discarded and cleaned files), shown in the centre.
    trash_view: Option<Entity<TrashView>>,
    _trash_events: Option<Subscription>,
    /// A ref compared with the current branch, shown in the centre instead of the graph.
    compare_view: Option<Entity<CompareView>>,
    _compare_events: Option<Subscription>,
    /// Where the open comparison's review session is saved (a branch compared with the
    /// checked-out one).
    compare_review_file: Option<PathBuf>,
    /// The review session as this tab last read or saved it: a save merges the tab's
    /// changes since onto the file as another Gitweft may have left it.
    compare_review_base: Option<ReviewSession>,
    /// The open diff came from the review, so its lines can be commented on.
    review_diff: bool,
    /// The open review diff shows what changed since the last review: it keeps its range
    /// and takes comments on its new side only.
    review_since: bool,
    /// The remote the next `RemoteOpKind::FetchOne` fetches.
    fetch_one: Option<String>,
    /// The submodules the next `SubmoduleUpdate` updates: the folder of the repository
    /// holding them (relative to the root; empty for this one) and their paths inside it
    /// (none: all).
    submodule_update: Option<(PathBuf, Vec<PathBuf>)>,
    /// A `git bisect run` going on, and the command used last (offered again).
    bisect_running: Option<bisect_run::BisectRunning>,
    /// What the last `git bisect run` printed (its banner's "Show output").
    last_bisect_output: Option<bisect_run::BisectOutput>,
    last_bisect_command: String,
    /// The interactive rebase editor; it sits right under the conflict tool.
    rebase_view: Option<Entity<RebaseView>>,
    _rebase_events: Option<Subscription>,
    /// The ref being compared, to compare again when the refs change.
    compare_head: Option<String>,
    /// What it's compared with: `None` for the checked-out branch (a review), else a
    /// ref or commit and the name shown.
    compare_base: Option<(String, String)>,
    /// Stops the running change search (pickaxe).
    pickaxe_cancel: Option<CancelToken>,
    /// Counts change searches, so results of a superseded one are dropped.
    pickaxe_generation: u64,
    /// Process start (first tab only); taken when the first repository frame is reported.
    started: Option<Instant>,
    /// Relays git's and ssh's credential prompts to dialogs (`None` if it couldn't start).
    _askpass: Option<AskpassServer>,
    /// Environment that routes every repository's prompts to the relay.
    askpass_env: Vec<(OsString, OsString)>,
    _askpass_task: Option<Task<()>>,
    /// Receives the toolbar's actions (and the menus', via their action context).
    focus: FocusHandle,
    /// The sidebar's lists (F6, the list keys), their keyboard cursor (an index into
    /// `sidebar_rows(false)`), and whether they had the focus when last drawn.
    sidebar_focus: FocusHandle,
    sidebar_cursor: Option<usize>,
    /// The row the sidebar cursor is on, so a reload or a filter that moves the rows leaves
    /// it on its branch.
    sidebar_cursor_row: Option<keyboard::SidebarRow>,
    /// The last reload's error while reloads keep failing: shown once, not on every retry.
    reload_error: Option<String>,
    /// The last `git status` error while it keeps failing: shown once, not on every refresh.
    status_error: Option<String>,
    sidebar_focused: bool,
    /// The lists' scrolling, and whether the cursor's row should scroll itself into view
    /// when drawn next (it moved by key).
    sidebar_scroll: ScrollHandle,
    sidebar_reveal: Rc<Cell<bool>>,
    /// The focus of a graph view a reload replaced: when it had the keys, the new view
    /// takes them on the next frame (the old view isn't drawn, so keys went nowhere).
    replaced_focus: Option<FocusHandle>,
    remote_op: Option<RemoteOp>,
    /// Debug builds: an operation to start on the next frame (`GITWEFT_DEBUG_REMOTE`).
    #[cfg_attr(not(debug_assertions), allow(dead_code))]
    debug_remote: Option<RemoteOpKind>,
    /// Upstream and ahead/behind per local branch (sidebar badges).
    /// By [`branch_key`]: a branch's name as `Tracking` has it (encoded when not UTF-8).
    tracking: HashMap<String, Tracking>,
    window_tasks: Vec<WindowTask>,
    /// The folder opened, created or cloned last: a tab whose open failed is named after it.
    last_path: Option<PathBuf>,
    /// The stash list, newest first (sidebar group, Pop button).
    stashes: Vec<StashEntry>,
    /// The configured remotes, with their URLs.
    remotes: Vec<RemoteInfo>,
    /// The repository's worktrees, the main one first.
    worktrees: Vec<WorktreeInfo>,
    /// The superproject's submodules, in path order.
    submodules: Vec<SubmoduleInfo>,
    /// Where the bisect in progress stands (read from git whenever the status shows one).
    bisect: Option<BisectState>,
    /// The remote picked for a branch's first push (`(branch, remote)`), when there are
    /// several and none is called origin. Kept for retries until the branch has an upstream.
    push_choice: Option<(String, String)>,
    /// Every git command run for this window's repositories.
    activity: ActivityLog,
    _activity_task: Task<()>,
    show_activity: bool,
    /// The activity panel also lists reads (status, log, diff…).
    activity_reads: bool,
    /// Local operations that can be undone, newest last; `redo_stack` holds undone ones.
    /// Each entry is ready for `undo::undo`, which returns the entry for the other stack.
    undo_stack: Vec<UndoEntry>,
    redo_stack: Vec<UndoEntry>,
    undo_running: bool,
    /// Where both stacks are kept across restarts; set once the repository has loaded.
    undo_file: Option<PathBuf>,
    /// Reads and writes of the undo log asked for so far; only the newest one's result
    /// is shown (`show_undo_history`).
    undo_syncs: u64,
    /// Recently opened repositories, newest first (start screen).
    recent: Vec<PathBuf>,
    /// The repository last put on the recent list, so reloads don't rewrite it.
    remembered_root: Option<PathBuf>,
    clone_op: Option<CloneOp>,
    /// LFS and shallow-clone banners, and the background fetch.
    sync: sync::SyncState,
    /// Gitweft's own files and how a repository loads; a test's are its own.
    env: WorkspaceEnv,
    /// The commit whose interdiff the file view shows, if it shows one.
    interdiff_commit: Option<ObjectId>,
    /// The start screen's list of recent repositories scrolls on its own, and GPUI doesn't
    /// scroll a focused element into view: its scroll, a focus handle per row (not tab stops;
    /// they only tell which row holds the focus) and the row last scrolled to.
    recent_scroll: ScrollHandle,
    recent_focus: Vec<FocusHandle>,
    recent_revealed: Option<usize>,
}

/// What a workspace reads outside the repository it shows: Gitweft's own files (today the
/// recently opened repositories) and how a repository is loaded. The app passes
/// [`WorkspaceEnv::app`]; a test passes a folder of its own, so it never reads or writes
/// the user's files (as [`LoadEnv`] already did for the loader).
#[derive(Debug, Clone)]
pub struct WorkspaceEnv {
    /// The recently opened repositories, shown on the start screen.
    pub recent_file: PathBuf,
    /// What a repository load may read besides the repository itself.
    pub load: LoadEnv,
}

impl WorkspaceEnv {
    /// The app's: its own data dir, and the config git itself reads.
    pub fn app() -> Self {
        Self {
            recent_file: recent::recent_file(),
            // The app keeps its files under the repository's gitweft.id.
            load: LoadEnv {
                assign_id: true,
                ..LoadEnv::app()
            },
        }
    }

    /// A test's: every file under `dir`, and the repository's own config only. This crate's
    /// own tests are the only ones that open a workspace, so it isn't built otherwise.
    #[cfg(test)]
    pub fn isolated(dir: &Path) -> Self {
        // Undo logs, repository settings and the trash are found by path, not through this
        // env: keep them out of the user's data dir too.
        let temp = std::env::temp_dir().join(format!("gitweft-test-data-{}", std::process::id()));
        gitweft_core::paths::use_data_dir_for_tests(temp);
        Self {
            recent_file: dir.join("recent-repositories"),
            load: LoadEnv {
                assign_id: true,
                ..LoadEnv::isolated(dir.to_owned())
            },
        }
    }
}

impl Workspace {
    /// The palette's labels as they stand (the headless UI tests).
    #[cfg(test)]
    pub(crate) fn palette_labels(&mut self, cx: &mut Context<Self>) -> Vec<String> {
        self.palette_commands(cx)
            .iter()
            .map(|command| command.label().to_string())
            .collect()
    }

    /// The git commands run so far (the headless UI tests).
    #[cfg(test)]
    pub(crate) fn activity(&self) -> Vec<gitweft_git::activity::ActivityEntry> {
        self.activity.entries()
    }

    /// The views open in the centre, by name, the file view with its title (the headless
    /// UI tests). The lists and the comparison count once their rows are read.
    #[cfg(test)]
    pub(crate) fn open_views(&self, cx: &App) -> Vec<String> {
        let mut open = Vec::new();
        let named = [
            // Listed once its rows are read: keys before that have nothing to move to.
            (
                self.tree_view
                    .as_ref()
                    .is_some_and(|view| view.read(cx).is_loaded()),
                "tree",
            ),
            (
                self.history_view
                    .as_ref()
                    .is_some_and(|view| view.read(cx).is_loaded()),
                "history",
            ),
            (self.diff_view.is_some(), "diff"),
            (
                self.reflog_view
                    .as_ref()
                    .is_some_and(|view| view.read(cx).is_loaded()),
                "reflog",
            ),
            (self.blame_view.is_some(), "blame"),
            (
                self.compare_view
                    .as_ref()
                    .is_some_and(|view| view.read(cx).is_loaded()),
                "compare",
            ),
        ];
        open.extend(
            named
                .iter()
                .filter(|(on, _)| *on)
                .map(|(_, name)| name.to_string()),
        );
        if let Some(file) = &self.file_view {
            open.push(format!("file {}", file.read(cx).title()));
        }
        open
    }

    /// The details panel (the headless UI tests).
    #[cfg(test)]
    pub(crate) fn details(&self) -> Entity<DetailsView> {
        self.details_view.clone()
    }

    /// Opens the diff of `path` in `commit` against `parent` (the headless UI tests).
    #[cfg(test)]
    pub(crate) fn open_commit_diff(
        &mut self,
        parent: Option<gitweft_git::ObjectId>,
        commit: gitweft_git::ObjectId,
        path: &str,
        cx: &mut Context<Self>,
    ) {
        self.open_diff(
            DiffParams {
                title: path.to_owned(),
                subtitle: String::new(),
                source: DiffSource::Commit { parent, commit },
                path: path.into(),
                old_path: None,
                options: DiffOptions::default(),
            },
            cx,
        );
    }

    /// Opens `path`'s blame at HEAD, or the file as of `commit` (the headless UI tests).
    #[cfg(test)]
    pub(crate) fn open_blame_or_file(
        &mut self,
        commit: Option<gitweft_git::ObjectId>,
        path: &str,
        cx: &mut Context<Self>,
    ) {
        match commit {
            None => self.open_blame(path.into(), None, cx),
            Some(commit) => self.open_file(commit, path.into(), cx),
        }
    }

    /// The commit graph, once the repository is loaded (the headless UI tests).
    #[cfg(test)]
    pub(crate) fn graph(&self) -> Option<Entity<GraphView>> {
        self.graph_view.clone()
    }

    /// The diff in the centre, if one is open (the headless UI tests).
    #[cfg(test)]
    pub(crate) fn diff(&self) -> Option<Entity<DiffView>> {
        self.diff_view.clone()
    }

    /// The staging panel (the headless UI tests).
    #[cfg(test)]
    pub(crate) fn staging(&self) -> Entity<StagingView> {
        self.staging_view.clone()
    }

    /// A tab showing `path` (the start screen when `None`); its jobs run on `pool`.
    pub fn new(
        path: Option<PathBuf>,
        started: Option<Instant>,
        pool: Arc<WorkerPool>,
        env: WorkspaceEnv,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let details_view = cx.new(|_| DetailsView::new());
        let details_events = cx.subscribe(&details_view, Self::on_details_event);
        let staging_view = cx.new(|cx| StagingView::new(window, cx));
        let conflict_panel = conflict_panel::ConflictPanel::new(window, cx);
        let staging_events = cx.subscribe(&staging_view, Self::on_staging_event);
        let search = cx.new(|cx| {
            InputState::new(window, cx).placeholder(t!("graph.search_placeholder").into_owned())
        });
        let sidebar_filter =
            cx.new(|cx| InputState::new(window, cx).placeholder(t!("sidebar.filter").into_owned()));
        let sidebar_filter_events =
            cx.subscribe(&sidebar_filter, |_, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            });
        // git and ssh run this very executable as GIT_ASKPASS/SSH_ASKPASS; it forwards each
        // prompt here, and it is answered in a dialog.
        let (askpass, askpass_env, askpass_task) =
            match (AskpassServer::start(), gitweft_git::helper_program()) {
                (Ok((server, requests)), Ok(exe)) => {
                    let env = server.git_env(&exe);
                    let task = cx.spawn_in(window, async move |this, cx| {
                        while let Ok(request) = requests.recv().await {
                            let shown = this.update_in(cx, |this, window, cx| {
                                this.open_askpass(request, window, cx)
                            });
                            if shown.is_err() {
                                break;
                            }
                        }
                    });
                    (Some(server), env, Some(task))
                }
                (Err(err), _) | (_, Err(err)) => {
                    eprintln!("gitweft: credential prompts are unavailable: {err}");
                    (None, Vec::new(), None)
                }
            };
        // The activity panel re-renders when commands finish (and only while it is open).
        let activity = ActivityLog::new();
        let activity_changes = activity.changes();
        let activity_task = cx.spawn(async move |this, cx| {
            while activity_changes.recv().await.is_ok() {
                let open = this.update(cx, |this, cx| {
                    if this.show_activity {
                        cx.notify();
                    }
                });
                if open.is_err() {
                    break;
                }
            }
        });
        let mut this = Self {
            state: RepoState::Empty,
            current_path: None,
            walk: None,
            tips: None,
            after_switch: None,
            commit_graph_banner: CommitGraphBanner::Hidden,
            search,
            graph_view: None,
            signatures: Default::default(),
            signature_queue: Vec::new(),
            signatures_reading: false,
            details_view,
            _details_events: details_events,
            _graph_events: None,
            diff_view: None,
            diff_params: None,
            _diff_events: None,
            blame_view: None,
            _blame_events: None,
            blame_cancel: None,
            blame_back: Vec::new(),
            history_view: None,
            _history_events: None,
            history_cancel: None,
            tree_view: None,
            _tree_events: None,
            file_view: None,
            _file_events: None,
            reflog_view: None,
            dangling_view: None,
            _dangling_events: None,
            dangling_cancel: None,
            trash_view: None,
            _trash_events: None,
            _reflog_events: None,
            reflog_ref: None,
            compare_view: None,
            _compare_events: None,
            compare_review_file: None,
            compare_review_base: None,
            review_diff: false,
            review_since: false,
            fetch_one: None,
            submodule_update: None,
            bisect_running: None,
            last_bisect_output: None,
            last_bisect_command: String::new(),
            rebase_view: None,
            _rebase_events: None,
            compare_head: None,
            compare_base: None,
            pickaxe_cancel: None,
            pickaxe_generation: 0,
            diff_layout: initial_diff_layout(cx),
            diff_ignore_whitespace: SettingsStore::settings(cx).diff.ignore_whitespace,
            highlight_dark: cx.palette().dark,
            highlight_tab: crate::code_scroll::tab_width(cx),
            sidebar_filter,
            _sidebar_filter_events: sidebar_filter_events,
            collapsed_remotes: HashSet::new(),
            panels: panels::Panels::default(),
            status: None,
            showing_wip: false,
            staging_view,
            _staging_events: staging_events,
            select_wip_when_dirty: false,
            visible: true,
            jobs: RepoJobs::new(pool),
            watcher: None,
            _watch_task: None,
            watch_limited: false,
            watch_stopped: false,
            untrusted: None,
            operation: None,
            conflict_view: None,
            _conflict_events: None,
            conflict_panel,
            started,
            _askpass: askpass,
            askpass_env,
            _askpass_task: askpass_task,
            focus: cx.focus_handle(),
            sidebar_focus: cx.focus_handle(),
            sidebar_cursor: None,
            sidebar_cursor_row: None,
            reload_error: None,
            status_error: None,
            sidebar_focused: false,
            sidebar_scroll: ScrollHandle::new(),
            sidebar_reveal: Rc::new(Cell::new(false)),
            replaced_focus: None,
            remote_op: None,
            debug_remote: None,
            tracking: HashMap::new(),
            window_tasks: Vec::new(),
            last_path: None,
            stashes: Vec::new(),
            remotes: Vec::new(),
            worktrees: Vec::new(),
            submodules: Vec::new(),
            bisect: None,
            push_choice: None,
            activity,
            _activity_task: activity_task,
            show_activity: false,
            activity_reads: false,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            undo_running: false,
            undo_file: None,
            undo_syncs: 0,
            recent: Vec::new(),
            remembered_root: None,
            clone_op: None,
            sync: sync::SyncState::default(),
            env,
            interdiff_commit: None,
            recent_scroll: ScrollHandle::new(),
            recent_focus: Vec::new(),
            recent_revealed: None,
        };
        this.update_recent(recent::load, Lane::Coalesce("recent-load"), cx);
        if let Some(path) = path {
            this.open(path, cx);
        }
        // Debug builds: `GITWEFT_DEBUG_CLONE=<url>|<dir>` clones right away, and
        // `GITWEFT_DEBUG_DIALOG=clone` opens the clone dialog (screenshot checks).
        #[cfg(debug_assertions)]
        if let Ok(spec) = std::env::var("GITWEFT_DEBUG_CLONE")
            && let Some((url, dir)) = spec.split_once('|')
        {
            this.start_clone(url.to_owned(), PathBuf::from(dir), false, cx);
        }
        #[cfg(debug_assertions)]
        if debug_dialog_once("clone") {
            this.with_window(|this, window, cx| this.prompt_clone(window, cx), cx);
        }
        this
    }

    /// With `GITWEFT_TRACE_STARTUP=1`, prints the time from process start to the first
    /// frame showing a repository; with `=exit`, also quits (for startup benchmarks).
    /// Whether this tab still has the startup time to trace (tests).
    #[cfg(test)]
    pub(crate) fn traces_startup(&self) -> bool {
        self.started.is_some()
    }

    fn trace_startup(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.state, RepoState::Loaded(_)) {
            return;
        }
        let Some(started) = self.started.take() else {
            return;
        };
        let Some(mode) = std::env::var_os("GITWEFT_TRACE_STARTUP") else {
            return;
        };
        eprintln!(
            "gitweft: first repository frame after {:.1} ms",
            started.elapsed().as_secs_f64() * 1000.0
        );
        if mode == "exit" {
            cx.quit();
        }
    }

    /// The tab's label: the repository's name, the folder a failed open was for, or "New
    /// tab" on the start screen.
    pub fn tab_label(&self) -> String {
        let folder = |path: &Path| {
            path.file_name().map_or_else(
                || path.display().to_string(),
                |name| name.to_string_lossy().into_owned(),
            )
        };
        match &self.state {
            RepoState::Loaded(repo) => repo.name.clone(),
            RepoState::Loading(path) => folder(path),
            RepoState::Failed(_) if self.last_path.is_some() => {
                folder(self.last_path.as_deref().unwrap_or(Path::new("")))
            }
            RepoState::Empty | RepoState::Failed(_) => t!("tabs.new_tab").into_owned(),
        }
    }

    /// Opening (or creating, or cloning) the repository failed: its tab says so.
    pub fn open_failed(&self) -> bool {
        matches!(self.state, RepoState::Failed(_)) && self.last_path.is_some()
    }

    /// Whether this tab shows (or is opening) the repository at `path`.
    /// Also a folder inside the repository's working tree that belongs to no repository of
    /// its own (`gitweft repo/src` brings this tab forward; a submodule or a nested
    /// repository opens its own).
    pub fn shows(&self, path: &Path) -> bool {
        let Some(root) = self.session_path() else {
            return false;
        };
        if root == path {
            return true;
        }
        let RepoState::Loaded(repo) = &self.state else {
            return false;
        };
        if repo.workdir.is_none() {
            return false;
        }
        in_worktree(&root, path)
    }

    /// The repository this tab reopens at the next start (`session.rs`): its working tree
    /// (its git dir when bare) or the folder being opened; nothing on a start screen.
    pub fn session_path(&self) -> Option<PathBuf> {
        match &self.state {
            RepoState::Loaded(repo) => Some(repo.cwd()),
            RepoState::Loading(path) => Some(path.clone()),
            RepoState::Empty | RepoState::Failed(_) => None,
        }
    }

    /// Shown next to the tabs while this one is active: the checked-out branch, or the
    /// folder being opened.
    pub fn status_line(&self) -> String {
        match &self.state {
            RepoState::Loaded(repo) => head_label(&repo.refs.head),
            RepoState::Loading(path) => t!("repo.opening", path = path.display()).into_owned(),
            RepoState::Empty | RepoState::Failed(_) => String::new(),
        }
    }

    /// The window title while this tab is active.
    pub fn window_title(&self) -> String {
        match &self.state {
            RepoState::Loaded(repo) => format!("{} — {APP_NAME}", repo.name),
            _ => APP_NAME.to_owned(),
        }
    }

    /// Receives the workspace's actions (toolbar menus, Ctrl+Z); the shell focuses it
    /// when the tab becomes active.
    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    /// Whether the tab is on screen: the active tab of an active window. Coming into view
    /// refreshes the status (the watcher keeps it current, this is the safety net) and,
    /// on the start screen, the recent list another tab may have changed. Polling, when
    /// watching is limited, runs only while visible.
    pub fn set_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if std::mem::replace(&mut self.visible, visible) || !visible {
            return;
        }
        // A tab watched in limited mode tries full watching again as it comes into view.
        if self.watch_limited
            && !self.watch_stopped
            && let Some(watcher) = &self.watcher
        {
            watcher.retry();
        }
        self.refresh_status(cx);
        if matches!(self.state, RepoState::Empty | RepoState::Failed(_)) {
            self.update_recent(recent::load, Lane::Coalesce("recent-load"), cx);
        }
    }

    /// A fetch, pull, push or clone is running; closing the tab would cancel it.
    pub fn busy(&self) -> bool {
        // A `git bisect run` checks out commit after commit: closing the tab stops it.
        self.remote_op.is_some() || self.clone_op.is_some() || self.bisect_running.is_some()
    }

    fn open(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.last_path = Some(path.clone());
        self.state = RepoState::Loading(path.clone());
        self.current_path = Some(path);
        self.walk = None;
        self.tips = None;
        self.commit_graph_banner = CommitGraphBanner::Hidden;
        self.graph_view = None;
        // Another repository verifies with its own config: its marks start afresh.
        self.signatures = Default::default();
        self.signature_queue.clear();
        self.close_diff();
        self.status = None;
        self.showing_wip = false;
        self.select_wip_when_dirty = true;
        self.watcher = None;
        self._watch_task = None;
        self.watch_limited = false;
        self.watch_stopped = false;
        self.untrusted = None;
        self.operation = None;
        self.conflict_view = None;
        self._conflict_events = None;
        self.conflict_panel.reset();
        self.close_centre();
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.last_bisect_output = None;
        self.undo_file = None;
        self.sync.reset();
        // What belonged to the last repository (or an earlier open of this one) goes too:
        // a push's remembered choice must never apply to another repository's branch.
        if let Some(running) = self.bisect_running.take() {
            running.cancel.cancel();
        }
        for cancel in [self.dangling_cancel.take(), self.pickaxe_cancel.take()]
            .into_iter()
            .flatten()
        {
            cancel.cancel();
        }
        self.push_choice = None;
        self.after_switch = None;
        self.fetch_one = None;
        self.submodule_update = None;
        self.sidebar_cursor = None;
        self.sidebar_cursor_row = None;
        self.collapsed_remotes.clear();
        self.stashes.clear();
        self.remotes.clear();
        self.worktrees.clear();
        self.submodules.clear();
        self.tracking.clear();
        self.bisect = None;
        self.reload_error = None;
        self.status_error = None;
        cx.notify();
        self.load(cx);
    }

    /// After a commit: reload the graph, then select the working changes if some remain.
    fn reload_after_commit(&mut self, cx: &mut Context<Self>) {
        self.close_diff();
        self.select_wip_when_dirty = true;
        self.load(cx);
    }

    /// Loads (or reloads) the repository at `current_path` in the background. The current
    /// view stays until the new one is ready, so a reload doesn't flash "Loading…".
    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.current_path.clone() else {
            return;
        };
        let (walk, tips) = (self.walk, self.tips.clone());
        let env = self.env.load.clone();
        // Opening another repository supersedes a load that is still running.
        let job = self.jobs.spawn(Lane::Latest("open-repo"), move |cancel| {
            load_repo_in(&path, &env, walk, tips, cancel)
        });
        cx.spawn(async move |this, cx| {
            let Some(result) = job.join().await else {
                return; // superseded by a newer open
            };
            this.update(cx, |this, cx| {
                match result {
                    Ok(mut repo) => {
                        for (key, value) in &this.askpass_env {
                            repo.git.set_env(key.clone(), value.clone());
                        }
                        repo.git.set_activity_log(this.activity.clone());
                        let metas = std::mem::take(&mut repo.metas);
                        let graph = repo.graph.clone();
                        let search = this.search.clone();
                        // A reload keeps the selected commit if it still exists.
                        let previous = this
                            .graph_view
                            .as_ref()
                            .and_then(|view| view.read(cx).selected_id())
                            .filter(|id| graph.row_of(id).is_some());
                        // The new graph starts without the change search; stop it.
                        this.stop_change_search();
                        this.walk = Some(repo.walk);
                        this.tips = Some(repo.tips.clone());
                        let (walk, tips) = (repo.walk, repo.tips.clone());
                        // Kept for the repository; read by now unless this is the first load
                        // (then the settings' read hands it over).
                        let width = this.sync.settings.graph_width;
                        let signatures = this.signatures.clone();
                        let view = cx.new(|cx| {
                            GraphView::new(graph, metas, search, walk, tips, width, signatures, cx)
                        });
                        // Not in a shallow clone (git ignores the file there), and not
                        // again once dismissed for the repository (the settings'
                        // read hides it too, if it comes after).
                        if repo.timings.walk_via_git
                            && repo.graph.rows() >= COMMIT_GRAPH_HINT_ROWS
                            && !repo.shallow
                            && !this.sync.settings.commit_graph_dismissed
                            && this.commit_graph_banner == CommitGraphBanner::Hidden
                        {
                            this.commit_graph_banner = CommitGraphBanner::Offer;
                        }
                        this._graph_events = Some(cx.subscribe(&view, Self::on_graph_event));
                        this.replaced_focus = this
                            .graph_view
                            .as_ref()
                            .map(|old| old.read(cx).focus_handle(cx));
                        this.graph_view = Some(view.clone());
                        this.push_bisect_marks(cx);
                        if this.watcher.is_none() {
                            this.start_watcher(&repo, cx);
                        }
                        let root = repo.cwd();
                        this.load_undo_history(&root, cx);
                        this.load_settings(&root, cx);
                        this.remember_recent(root, cx);
                        this.enforce_trash_limits(cx);
                        let head = head_oid(&repo.refs.head);
                        this.state = RepoState::Loaded(Box::new(repo));
                        this.reload_error = None;
                        this.push_worktrees(cx);
                        if let Some(id) = previous {
                            view.update(cx, |view, cx| view.restore_selection(id, cx));
                            // A reload keeps one of several selected commits: show it
                            // (the picked ones mustn't be offered again).
                            if this.details_view.read(cx).shows_several() {
                                this.show_details(id, cx);
                            }
                        } else if this.showing_wip {
                            // The status that follows re-selects the working changes, or
                            // HEAD if they are gone.
                            this.select_wip_when_dirty = true;
                        } else {
                            // Start with the checked-out commit selected, like the details
                            // panel of any Git client would show on open.
                            this.details_view.update(cx, |details, cx| {
                                details.set_state(DetailsState::Empty, cx)
                            });
                            // Selected without an event: a selection event closes the
                            // views in the centre, and a reload (say, after a reset in a
                            // terminal) must not close an open reflog or history.
                            if let Some(head) = head {
                                view.update(cx, |view, cx| view.restore_selection(head, cx));
                                this.show_details(head, cx);
                            }
                        }
                        this.load_more_meta(cx);
                        this.refresh_status(cx);
                        this.load_co_authors(cx);
                        this.load_tracking(cx);
                        this.load_stashes(cx);
                        this.load_remotes(cx);
                        this.load_worktrees(cx);
                        this.load_submodules(cx);
                        this.load_extras(cx);
                        // The lost marks follow the refs.
                        this.load_reflog(cx);
                        this.load_compare(cx);
                        #[cfg(debug_assertions)]
                        if debug_dialog_once("stash") {
                            this.with_window(
                                |this, window, cx| this.open_stash_dialog(Vec::new(), window, cx),
                                cx,
                            );
                        }
                        #[cfg(debug_assertions)]
                        if std::env::var_os("GITWEFT_DEBUG_ACTIVITY").is_some() {
                            this.show_activity = true;
                        }
                        #[cfg(debug_assertions)]
                        if debug_dialog_once("new-branch") {
                            this.with_window(
                                |this, window, cx| this.open_new_branch(None, window, cx),
                                cx,
                            );
                        }
                        #[cfg(debug_assertions)]
                        this.debug_askpass(cx);
                        #[cfg(debug_assertions)]
                        {
                            this.debug_remote = debug_remote_kind();
                        }
                        #[cfg(debug_assertions)]
                        this.debug_undo(cx);
                    }
                    Err(message) => {
                        // Git refused a folder another user owns: offer to trust it.
                        this.untrusted = gitweft_git::repair::untrusted_folder(
                            &message,
                            this.current_path.as_deref(),
                        );
                        if matches!(this.state, RepoState::Loaded(_)) {
                            // A reload that failed (a lock, a half-written ref) keeps what's
                            // shown, and the next change the watcher sees tries again.
                            if this.reload_error.as_deref() != Some(message.as_str()) {
                                let text =
                                    t!("repo.reload_failed", error = message.clone()).into_owned();
                                crate::tab_notes::note_soon(Toast::warning(text), cx);
                            }
                            this.reload_error = Some(message);
                        } else {
                            this.state = RepoState::Failed(message);
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Re-reads `git status` in the background and updates the WIP row and, if shown,
    /// the working-changes panel. Bursts of requests coalesce into one run.
    fn refresh_status(&mut self, cx: &mut Context<Self>) {
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let Some(workdir) = repo.workdir.clone() else {
            // A bare repository: no working tree, but git may still refuse its folder.
            self.check_bare_trust(cx);
            return;
        };
        let git = repo.git.clone();
        let shared = repo.repo.clone();
        let known = self.conflict_panel.files().to_vec();
        let job = self.jobs.spawn(Lane::Coalesce("status"), move |cancel| {
            // Keep git's error (its kind says why), whatever else a read can fail with.
            let status = read_status(&git, &workdir, cancel).map_err(|err| {
                err.downcast::<GitError>().map_or_else(
                    |other| GitError::new(GitErrorKind::Other, "git status", other.to_string()),
                    |git| *git,
                )
            })?;
            // For the commit box: HEAD's message (amend prefill), identity and template.
            let has_head = status.branch.oid.is_some();
            let context = gitweft_git::ops::commit_context(&git, &workdir, has_head, cancel);
            // A merge or rebase in progress, whoever started it (the operation banner).
            let local = shared.to_thread_local();
            let operation = operation_state(&local);
            // The conflicted-files panel: conflicts left per file, the merge message.
            let conflicts = conflict_panel::read_conflicts(
                &git, &local, &workdir, &status, operation, known, cancel,
            );
            Ok::<_, GitError>((status, context, operation, conflicts))
        });
        let view_id = self.graph_view.as_ref().map(Entity::entity_id);
        cx.spawn(async move |this, cx| {
            let (status, context, operation, conflicts) = match job.join().await {
                Some(Ok(result)) => result,
                Some(Err(err)) if err.kind == GitErrorKind::DubiousOwnership => {
                    // gix read the repository, but git refuses its folder: say why.
                    this.update(cx, |this, cx| {
                        let opened = this.current_path.clone();
                        this.untrusted =
                            gitweft_git::repair::untrusted_folder(&err.stderr, opened.as_deref())
                                .or(opened);
                        cx.notify();
                    })
                    .ok();
                    return;
                }
                Some(Err(err)) => {
                    // Said once per error, not on every refresh.
                    let message = err.summary();
                    this.update(cx, |this, cx| {
                        if this.status_error.as_deref() != Some(message.as_str()) {
                            let text = t!("status.failed", error = message.clone()).into_owned();
                            crate::tab_notes::note_failure(text, cx);
                        }
                        this.status_error = Some(message);
                        this.staging_view
                            .update(cx, |staging, cx| staging.set_stale(cx));
                    })
                    .ok();
                    return;
                }
                None => return,
            };
            this.update(cx, |this, cx| {
                this.status_error = None;
                // Drop results for a repository that has been replaced meanwhile.
                if this.graph_view.as_ref().map(Entity::entity_id) != view_id {
                    return;
                }
                if this.operation != operation {
                    this.operation = operation;
                    cx.notify();
                }
                // A bisect's state lives in git (the user may bisect in a terminal too).
                if matches!(operation, Some(Operation::Bisect)) {
                    this.load_bisect(cx);
                } else if this.bisect.take().is_some() {
                    this.push_bisect_marks(cx);
                }
                if this.conflict_panel.update(operation, &status, conflicts) {
                    cx.notify();
                }
                // The files listed survive a restart (git's own list isn't always there).
                if let Some(record) = this.conflict_panel.record_change() {
                    this.save_conflict_record(record, cx);
                }
                let unchanged = this.status.as_ref() == Some(&status);
                if unchanged && !this.select_wip_when_dirty {
                    // Nothing to redraw (the watcher reports every write, even ones that
                    // leave the status as it was); only the commit context may be new.
                    this.staging_view.update(cx, |staging, cx| {
                        staging.set_commit_context(context, cx);
                    });
                    return;
                }
                // A submodule moved (in a terminal, or in its own tab) shows up in the
                // status, not in the refs: read the list again.
                if !this.submodules.is_empty() || status.entries.iter().any(|e| e.submodule) {
                    this.load_submodules(cx);
                }
                if let Some(watcher) = &this.watcher {
                    let untracked = status
                        .entries
                        .iter()
                        .filter(|e| e.kind == EntryKind::Untracked)
                        .map(|e| e.path.clone())
                        .collect();
                    watcher.set_untracked(untracked);
                }
                if let Some(view) = this.graph_view.clone() {
                    let wip = WipSummary::from_status(&status);
                    // Right after opening, uncommitted changes are what the user most
                    // likely wants to act on: select them instead of HEAD.
                    let select_wip =
                        std::mem::take(&mut this.select_wip_when_dirty) && wip.is_some();
                    // The working changes were shown but are gone (committed, discarded or
                    // stashed elsewhere): show the checked-out commit instead. HEAD comes
                    // from this status: after an operation that committed, the refs (and
                    // the graph) aren't re-read yet, and the reload that brings the new
                    // commit selects it.
                    let reveal_head = (this.showing_wip && wip.is_none())
                        .then_some(status.branch.oid)
                        .flatten();
                    view.update(cx, |view, cx| {
                        view.set_wip(wip, cx);
                        if select_wip {
                            view.select_wip(cx);
                            // They sit above the first row (the load centred HEAD).
                            view.scroll_to_top(cx);
                        } else if let Some(head) = reveal_head {
                            view.reveal(head, cx);
                        }
                    });
                }
                let for_staging = status.clone();
                this.staging_view.update(cx, |staging, cx| {
                    staging.set_status(for_staging, cx);
                    staging.set_commit_context(context, cx);
                });
                let first_status = this.status.is_none();
                this.status = Some(status);
                this.follow_moved_file(cx);
                #[cfg(debug_assertions)]
                if first_status {
                    this.debug_open_diff(cx);
                }
                #[cfg(not(debug_assertions))]
                let _ = first_status;
            })
            .ok();
        })
        .detach();
    }

    /// Shows a credential prompt from git or ssh. The request is answered at most once;
    /// closing the dialog any other way drops it, which git sees as "cancelled".
    fn open_askpass(
        &mut self,
        request: AskpassRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let kind = request.kind();
        let prompt = request.prompt.trim().to_owned();
        let pending = Rc::new(RefCell::new(Some(request)));
        let input = (kind != PromptKind::Confirm)
            .then(|| cx.new(|cx| InputState::new(window, cx).masked(kind == PromptKind::Secret)));
        let title = match kind {
            PromptKind::Secret => t!("askpass.title_secret"),
            PromptKind::Text => t!("askpass.title_text"),
            PromptKind::Confirm => t!("askpass.title_host"),
        }
        .into_owned();
        let ok_text = match kind {
            PromptKind::Confirm => t!("askpass.connect"),
            PromptKind::Secret | PromptKind::Text => t!("askpass.ok"),
        }
        .into_owned();

        let dialog_input = input.clone();
        let dialog_bg = cx.palette().panel;
        window.open_dialog(cx, move |dialog, _, _| {
            // On the panel color, like the prompts, menus and toasts.
            let dialog = dialog.bg(dialog_bg);
            let (on_ok, on_cancel, on_close) = (pending.clone(), pending.clone(), pending.clone());
            let ok_input = dialog_input.clone();
            let (prompt, input) = (prompt.clone(), dialog_input.clone());
            dialog
                .title(title.clone())
                .overlay_closable(false)
                .content(move |content, _, cx| {
                    content
                        .gap_3()
                        .child(
                            gitweft_ui::flow::flow("text-dialog-prompt", prompt.clone())
                                .text_size(cx.metrics().text.small),
                        )
                        .when_some(input.clone(), |this, input| this.child(Input::new(&input)))
                })
                // A plain Dialog draws only the footer it is given.
                .footer(
                    DialogFooter::new()
                        .child(
                            DialogClose::new()
                                .trigger(|button| button.label(t!("askpass.cancel").into_owned())),
                        )
                        .child(
                            DialogAction::new()
                                .child(Button::new("askpass-ok").primary().label(ok_text.clone())),
                        ),
                )
                // However the dialog goes away (×, Esc, Cancel), git must get an answer.
                .on_close(move |_, _, _| {
                    if let Some(request) = on_close.borrow_mut().take() {
                        request.cancel();
                    }
                })
                .on_ok(move |_, _, cx| {
                    // A host-key question is answered "yes"; everything else with the input.
                    let answer = match &ok_input {
                        Some(input) => input.read(cx).value().to_string(),
                        None => "yes".to_owned(),
                    };
                    if let Some(request) = on_ok.borrow_mut().take() {
                        request.answer(answer);
                    }
                    true
                })
                .on_cancel(move |_, _, _| {
                    if let Some(request) = on_cancel.borrow_mut().take() {
                        request.cancel();
                    }
                    true
                })
        });
        // Enter answers through the dialog's own Enter → Confirm → `on_ok`: the input lets
        // Enter propagate, so a PressEnter handler of ours would answer a second time.
        if let Some(input) = &input {
            input.update(cx, |input, cx| input.focus(window, cx));
        }
    }

    /// Debug builds: `GITWEFT_DEBUG_ASKPASS=1` asks git for credentials of a made-up host
    /// once after opening, so the prompt dialog can be checked without a real server.
    #[cfg(debug_assertions)]
    fn debug_askpass(&mut self, _: &mut Context<Self>) {
        use std::sync::atomic::{AtomicBool, Ordering};
        static DONE: AtomicBool = AtomicBool::new(false);
        if std::env::var_os("GITWEFT_DEBUG_ASKPASS").is_none() || DONE.swap(true, Ordering::Relaxed)
        {
            return;
        }
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let git = repo.git.clone();
        let cwd = repo.cwd();
        self.jobs
            .spawn(Lane::Latest("debug-askpass"), move |cancel| {
                let _ = git
                    .command(&cwd)
                    .args(["credential", "fill"])
                    .stdin("protocol=https\nhost=git.example.invalid\n\n")
                    .run(cancel);
            });
    }

    /// Recent authors for the co-author picker, read once per (re)load.
    fn load_co_authors(&mut self, cx: &mut Context<Self>) {
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let Some(workdir) = repo.workdir.clone() else {
            return;
        };
        let git = repo.git.clone();
        let job = self.jobs.spawn(Lane::Latest("co-authors"), move |cancel| {
            recent_authors(&git, &workdir, CO_AUTHOR_HISTORY, cancel).unwrap_or_default()
        });
        let staging = self.staging_view.clone();
        cx.spawn(async move |_, cx| {
            if let Some(people) = job.join().await {
                staging.update(cx, |staging, cx| {
                    staging.set_co_author_candidates(people, cx)
                });
            }
        })
        .detach();
    }

    /// Starts watching the repository; its updates drive [`Self::on_watch`].
    fn start_watcher(&mut self, repo: &LoadedRepo, cx: &mut Context<Self>) {
        let (watcher, updates) = RepoWatcher::start(repo.git.clone(), repo.repo.clone());
        self.watcher = Some(watcher);
        self._watch_task = Some(cx.spawn(async move |this, cx| {
            while let Ok(update) = updates.recv().await {
                if this
                    .update(cx, |this, cx| this.on_watch(update, cx))
                    .is_err()
                {
                    break;
                }
            }
        }));
    }

    /// The refresh planner: maps what changed on disk to the cheapest refresh that covers it.
    fn on_watch(&mut self, update: Update, cx: &mut Context<Self>) {
        match update {
            Update::Ready => {}
            Update::Changed(changes) => {
                if changes.refs {
                    self.refresh_refs(cx);
                    // refs/stash isn't part of the graph's refs; reread the list.
                    self.load_stashes(cx);
                    self.load_remotes(cx);
                    self.load_worktrees(cx);
                    self.load_submodules(cx);
                } else if changes.submodules {
                    // A submodule moved or was set up: its row's state changes.
                    self.load_submodules(cx);
                }
                if changes.status {
                    self.refresh_status(cx);
                    if self.diff_shows_worktree() {
                        self.reload_diff(cx);
                    }
                }
            }
            Update::Limited(message) => {
                eprintln!("gitweft: file watching limited: {message}");
                self.watch_limited = true;
                cx.notify();
            }
            Update::Stopped => {
                eprintln!("gitweft: file watching stopped; the status is polled instead");
                self.watch_limited = true;
                self.watch_stopped = true;
                cx.notify();
            }
            Update::Poll => {
                if self.visible {
                    self.refresh_status(cx);
                }
            }
            // The working tree is watched again: the banner goes.
            Update::Recovered => {
                self.watch_limited = false;
                cx.notify();
            }
        }
    }

    /// Re-reads the refs (a few ms) and reloads the graph only if they differ from what
    /// is shown, so our own writes, reflog updates and no-op fetches cost no reload.
    fn refresh_refs(&mut self, cx: &mut Context<Self>) {
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let shared = repo.repo.clone();
        let git = repo.git.clone();
        let shown = repo.refs.clone();
        let job = self
            .jobs
            .spawn(Lane::Coalesce("refs-check"), move |cancel| {
                // A reftable repository's refs come from git.
                read_refs_with(&shared.to_thread_local(), &git, cancel)
                    .map_or(true, |refs| refs != shown)
            });
        let view_id = self.graph_view.as_ref().map(Entity::entity_id);
        cx.spawn(async move |this, cx| {
            if job.join().await != Some(true) {
                return;
            }
            this.update(cx, |this, cx| {
                if this.graph_view.as_ref().map(Entity::entity_id) == view_id {
                    this.load(cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// The open diff compares against the working tree or index (not a commit).
    fn diff_shows_worktree(&self) -> bool {
        self.diff_params
            .as_ref()
            .is_some_and(|params| !matches!(params.source, DiffSource::Commit { .. }))
    }

    /// After a status read: the open diff of a working-changes file that moved to the other
    /// list (staged or unstaged whole, here or in a terminal) shows its new side, and the
    /// staging list selects the file there.
    fn follow_moved_file(&mut self, cx: &mut Context<Self>) {
        let (Some(params), Some(status)) = (&self.diff_params, &self.status) else {
            return;
        };
        let staged = match params.source {
            DiffSource::Staged => true,
            DiffSource::Unstaged | DiffSource::Untracked => false,
            DiffSource::Commit { .. } => return,
        };
        let Some((entry, staged)) = crate::staging_view::moved_to(status, &params.path, staged)
        else {
            return;
        };
        // The new side takes the keys only if the diff it replaces had them (a hunk staged
        // from the diff); after a stage from the list they stay in the list.
        let had_focus = self
            .diff_view
            .as_ref()
            .is_some_and(|view| view.read(cx).has_focus());
        self.open_status_diff(&entry, staged, cx);
        if let Some(view) = &self.diff_view {
            view.update(cx, |view, _| view.set_focus_on_open(had_focus));
        }
        self.staging_view.update(cx, |staging, cx| {
            staging.select_file(&entry.path, staged, cx)
        });
    }

    /// Loads commit metadata for the next chunk of rows, then schedules the following one.
    fn load_more_meta(&mut self, cx: &mut Context<Self>) {
        let (RepoState::Loaded(repo), Some(view)) = (&self.state, &self.graph_view) else {
            return;
        };
        let start = view.read(cx).loaded_rows();
        let rows = repo.graph.rows();
        if start >= rows {
            return;
        }
        let ids = repo
            .graph
            .commits
            .range(start..rows.min(start + META_CHUNK_ROWS));
        let shared_repo = repo.repo.clone();
        let view = view.clone();
        let job = self.jobs.spawn(Lane::Coalesce("commit-meta"), move |_| {
            load_meta_chunk(&shared_repo, &ids)
        });
        cx.spawn(async move |this, cx| {
            let Some(metas) = job.join().await else {
                return;
            };
            this.update(cx, |this, cx| {
                // Drop results for a repository that has been replaced meanwhile.
                if this.graph_view.as_ref().map(Entity::entity_id) != Some(view.entity_id()) {
                    return;
                }
                view.update(cx, |view, cx| view.append_metas(metas, cx));
                this.load_more_meta(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Reads the signature marks of the queued commits: one read at a time, taking every
    /// commit queued so far (rows scrolled into view meanwhile wait for the next).
    fn read_signatures(&mut self, cx: &mut Context<Self>) {
        if self.signatures_reading || self.signature_queue.is_empty() {
            return;
        }
        let RepoState::Loaded(repo) = &self.state else {
            self.signature_queue.clear();
            return;
        };
        let ids = std::mem::take(&mut self.signature_queue);
        let (git, git_dir) = (repo.git.clone(), repo.repo.git_dir().to_owned());
        let shared_repo = repo.repo.clone();
        // A result lands in the cache it was asked for, even if another repository opened.
        let cache = self.signatures.clone();
        self.signatures_reading = true;
        let job = self
            .jobs
            .spawn(Lane::Coalesce("signatures"), move |cancel| {
                let repo = shared_repo.to_thread_local();
                gitweft_git::read::signature_marks(&git, &git_dir, &repo, &ids, cancel)
                    .unwrap_or_default()
            });
        cx.spawn(async move |this, cx| {
            let marks = job.join().await.unwrap_or_default();
            this.update(cx, |this, cx| {
                this.signatures_reading = false;
                // Unsigned rows draw nothing new; a signed one needs its mark drawn.
                let signed = marks.iter().any(|(_, mark)| mark.is_some());
                cache.borrow_mut().extend(marks);
                if signed && let Some(view) = &this.graph_view {
                    view.update(cx, |_, cx| cx.notify());
                }
                this.read_signatures(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Selects `id` in the graph without closing the views in the centre, and shows its
    /// details: the graph's selection and the details stay the same commit.
    fn select_quietly(&mut self, id: ObjectId, cx: &mut Context<Self>) {
        if let Some(graph) = self.graph_view.clone() {
            graph.update(cx, |graph, cx| graph.restore_selection(id, cx));
        }
        self.show_details(id, cx);
    }

    /// Selects `id` in the graph, which closes the views shown instead of it. A
    /// notification says so if the commit isn't in the graph.
    fn show_commit(&mut self, id: ObjectId, cx: &mut Context<Self>) {
        let revealed = self
            .graph_view
            .clone()
            .is_some_and(|graph| graph.update(cx, |graph, cx| graph.reveal(id, cx)));
        if !revealed {
            self.with_window(
                |_, _, cx| {
                    let message = t!("graph.commit_not_shown").into_owned();
                    note(Toast::warning(message), cx);
                },
                cx,
            );
        }
    }

    fn on_details_event(
        &mut self,
        _: Entity<DetailsView>,
        event: &DetailsEvent,
        cx: &mut Context<Self>,
    ) {
        let (commit, parent, change) = match event {
            DetailsEvent::OpenFile {
                commit,
                parent,
                change,
            } => (commit, parent, change),
            DetailsEvent::Blame { commit, path } => {
                self.open_blame(path.clone(), Some(*commit), cx);
                return;
            }
            DetailsEvent::History { commit, path } => {
                self.open_history(path.clone(), Some(*commit), cx);
                return;
            }
            DetailsEvent::BrowseFiles(commit) => {
                self.open_tree(*commit, cx);
                return;
            }
            DetailsEvent::SelectCommit(id) => {
                if let Some(graph) = &self.graph_view {
                    graph.update(cx, |graph, cx| {
                        graph.reveal(*id, cx);
                    });
                }
                return;
            }
            DetailsEvent::Preview {
                commit,
                parent,
                change,
            } => {
                self.load_preview(*commit, *parent, change.clone(), cx);
                return;
            }
            DetailsEvent::Close => {
                self.close_details(cx);
                return;
            }
            DetailsEvent::StashApply { entry, pop } => {
                let (entry, pop) = (entry.clone(), *pop);
                self.with_window(
                    move |this, window, cx| this.apply_stash(entry, pop, window, cx),
                    cx,
                );
                return;
            }
            DetailsEvent::StashDrop(entry) => {
                let entry = entry.clone();
                self.with_window(
                    move |this, window, cx| this.confirm_drop_stash(entry, window, cx),
                    cx,
                );
                return;
            }
            DetailsEvent::CherryPick(commits) => {
                let commits = commits.clone();
                self.with_window(
                    move |this, window, cx| this.pick_commits(commits, false, window, cx),
                    cx,
                );
                return;
            }
            DetailsEvent::SavePatches(commits) => {
                let commits = commits.clone();
                self.with_window(
                    move |this, window, cx| this.save_patches(commits, window, cx),
                    cx,
                );
                return;
            }
            DetailsEvent::FileMenu {
                commit,
                parent,
                change,
            } => {
                let (commit, parent, change) = (*commit, *parent, change.clone());
                self.with_window(
                    move |this, window, cx| {
                        this.open_details_file_menu(commit, parent, change, window, cx)
                    },
                    cx,
                );
                return;
            }
            DetailsEvent::DiffTool {
                commit,
                parent,
                change,
            } => {
                let target = gitweft_git::tools::DiffToolTarget::Commit {
                    parent: *parent,
                    commit: *commit,
                };
                let path = change.path.clone();
                let old = change.old_path.clone();
                self.with_window(
                    move |this, window, cx| this.run_diff_tool(target, path, old, window, cx),
                    cx,
                );
                return;
            }
            DetailsEvent::WorkingFile { path, editor } => {
                let program = if *editor {
                    external::Program::Editor
                } else {
                    external::Program::FileManager
                };
                let path = path.clone();
                self.with_window(
                    move |this, window, cx| this.open_working_file(&path, program, window, cx),
                    cx,
                );
                return;
            }
        };
        // A stash's file (its untracked ones come from a commit of its own): name the stash.
        let subtitle = match self.details_view.read(cx).shown_stash() {
            Some(stash) => t!("diff.in_stash", name = stash.revision()),
            None => t!("diff.in_commit", sha = commit.to_hex_with_len(8)),
        }
        .into_owned();
        let source = DiffSource::Commit {
            parent: *parent,
            commit: *commit,
        };
        let params = DiffParams {
            title: change.path.to_string(),
            subtitle,
            source,
            path: change.path.clone(),
            old_path: change.old_path.clone(),
            options: DiffOptions::default(),
        };
        self.open_diff(params, cx);
    }

    /// Opens the diff of a working-tree file; a conflicted one opens the conflict tool.
    fn open_status_diff(&mut self, entry: &StatusEntry, staged: bool, cx: &mut Context<Self>) {
        if entry.kind == EntryKind::Conflicted {
            let path = entry.path.clone();
            self.with_window(
                move |this, window, cx| this.open_conflict(path, window, cx),
                cx,
            );
            return;
        }
        let (source, subtitle) = if staged {
            (DiffSource::Staged, t!("diff.staged"))
        } else if entry.kind == EntryKind::Untracked {
            (DiffSource::Untracked, t!("diff.untracked"))
        } else {
            (DiffSource::Unstaged, t!("diff.unstaged"))
        };
        let params = DiffParams {
            title: entry.display_path(),
            subtitle: subtitle.into_owned(),
            source,
            path: entry.path.clone(),
            old_path: if staged {
                entry.orig_path.clone()
            } else {
                None
            },
            options: DiffOptions::default(),
        };
        self.open_diff(params, cx);
    }

    fn on_graph_event(&mut self, _: Entity<GraphView>, event: &GraphEvent, cx: &mut Context<Self>) {
        // A commit revealed from the sidebar needs the graph visible again.
        if matches!(event, GraphEvent::Selected(_)) && self.centre_covered() {
            self.close_centre();
            cx.notify();
        }
        let id = match event {
            GraphEvent::Selected(id) => *id,
            GraphEvent::BrowseFiles(id) => {
                self.open_tree(*id, cx);
                return;
            }
            GraphEvent::SearchChanges(query) => {
                self.start_change_search(query.clone(), cx);
                return;
            }
            GraphEvent::CancelChangeSearch => {
                self.stop_change_search();
                return;
            }
            GraphEvent::WipSelected => {
                self.showing_wip = true;
                self.refresh_status(cx);
                cx.notify();
                return;
            }
            // Menu actions need the window for dialogs and notifications.
            GraphEvent::CreateBranchAt(id) => {
                let start = (id.to_string(), id.to_hex_with_len(8).to_string());
                self.with_window(
                    move |this, window, cx| this.open_new_branch(Some(start), window, cx),
                    cx,
                );
                return;
            }
            GraphEvent::CheckoutCommit(id) => {
                let op = BranchOp::Detach { commit: *id };
                self.with_window(
                    move |this, window, cx| this.run_branch_op(op, window, cx),
                    cx,
                );
                return;
            }
            GraphEvent::CreateTagAt(id) => {
                let id = *id;
                self.with_window(
                    move |this, window, cx| this.open_new_tag(id, window, cx),
                    cx,
                );
                return;
            }
            GraphEvent::SavePatch(id) => {
                let id = *id;
                self.with_window(move |this, window, cx| this.save_patch(id, window, cx), cx);
                return;
            }
            GraphEvent::Archive(id) => {
                let id = *id;
                self.with_window(
                    move |this, window, cx| this.export_archive(id, window, cx),
                    cx,
                );
                return;
            }
            GraphEvent::Bisect(id, mark) => {
                let (id, mark) = (*id, *mark);
                self.with_window(
                    move |this, window, cx| this.bisect_from_graph(id, mark, window, cx),
                    cx,
                );
                return;
            }
            GraphEvent::CherryPick(commit) | GraphEvent::Revert(commit) => {
                let (commit, reverting) = (commit.clone(), matches!(event, GraphEvent::Revert(_)));
                self.with_window(
                    move |this, window, cx| this.pick_commit(commit, reverting, window, cx),
                    cx,
                );
                return;
            }
            GraphEvent::NeedSignatures(ids) => {
                self.signature_queue.extend(ids.iter().copied());
                self.read_signatures(cx);
                return;
            }
            GraphEvent::CherryPickNoCommit(commit) => {
                let commit = commit.clone();
                self.with_window(
                    move |this, window, cx| this.pick_without_commit(commit, window, cx),
                    cx,
                );
                return;
            }
            GraphEvent::Reset(commit, mode) => {
                let (commit, mode) = (commit.clone(), *mode);
                self.with_window(
                    move |this, window, cx| this.reset_to(commit, mode, window, cx),
                    cx,
                );
                return;
            }
            GraphEvent::DropCommits(commits) => {
                let mut commits = commits.clone();
                self.with_window(
                    move |this, window, cx| {
                        if commits.len() == 1 {
                            this.confirm_drop_pick(commits.remove(0), window, cx)
                        } else {
                            this.confirm_drop_picks(commits, window, cx)
                        }
                    },
                    cx,
                );
                return;
            }
            GraphEvent::MultiSelected(commits) => {
                if self.centre_covered() {
                    self.close_centre();
                }
                self.showing_wip = false;
                let state = DetailsState::Several(commits.clone());
                self.details_view
                    .update(cx, |details, cx| details.set_state(state, cx));
                cx.notify();
                return;
            }
            GraphEvent::CherryPickMany(commits) => {
                let commits = commits.clone();
                self.with_window(
                    move |this, window, cx| this.pick_commits(commits, false, window, cx),
                    cx,
                );
                return;
            }
            GraphEvent::SavePatches(commits) => {
                let commits = commits.clone();
                self.with_window(
                    move |this, window, cx| this.save_patches(commits, window, cx),
                    cx,
                );
                return;
            }
            // The merge previews and asks, as from the sidebar.
            GraphEvent::DropBranchOnBranch {
                full_name,
                shown,
                targets,
            } => {
                let (full_name, shown, targets) =
                    (full_name.clone(), shown.clone(), targets.clone());
                self.with_window(
                    move |this, window, cx| {
                        this.drop_branch_on_branches(full_name, shown, targets, window, cx)
                    },
                    cx,
                );
                return;
            }
            GraphEvent::ShowHidden(entry) => {
                self.show_hidden(entry.clone(), cx);
                return;
            }
            GraphEvent::GraphWidth(width) => {
                self.set_graph_width(*width, cx);
                return;
            }
            GraphEvent::DropBranch { full_name, shown } => {
                let (revision, shown) = (full_name.clone(), shown.clone());
                self.with_window(
                    move |this, window, cx| this.merge_into_current(revision, shown, window, cx),
                    cx,
                );
                return;
            }
            GraphEvent::InteractiveRebase(commit) | GraphEvent::InteractiveRebaseFrom(commit) => {
                let include = matches!(event, GraphEvent::InteractiveRebaseFrom(_));
                let commit = commit.clone();
                self.with_window(
                    move |this, window, cx| this.open_rebase(commit.id, include, window, cx),
                    cx,
                );
                return;
            }
            GraphEvent::RowMenu => {
                self.with_window(|this, window, cx| this.open_commit_menu(window, cx), cx);
                return;
            }
            GraphEvent::Activated(id) => {
                let id = *id;
                self.with_window(
                    move |this, window, cx| this.checkout_from_graph(id, window, cx),
                    cx,
                );
                return;
            }
            GraphEvent::SelectMergeBase(id) => {
                self.select_merge_base(*id, cx);
                return;
            }
            GraphEvent::CompareWith(id) => {
                let (head, name) = (id.to_string(), id.to_hex_with_len(8).to_string());
                self.with_window(
                    move |this, window, cx| {
                        this.choose_compare_base(head.clone(), name.clone(), window, cx)
                    },
                    cx,
                );
                return;
            }
            GraphEvent::SetWalk(walk) => {
                self.set_walk(*walk, cx);
                return;
            }
            GraphEvent::SetScope(current_branch) => {
                self.set_scope(*current_branch, cx);
                return;
            }
            GraphEvent::ShowEveryBranch => {
                self.show_every_branch(cx);
                return;
            }
            GraphEvent::QuickRebase(commit, edit) => {
                let (commit, edit) = (commit.clone(), *edit);
                self.with_window(
                    move |this, window, cx| this.quick_rebase(commit, edit, window, cx),
                    cx,
                );
                return;
            }
            GraphEvent::CreateFixup(commit) => {
                let commit = commit.clone();
                self.with_window(
                    move |this, window, cx| this.create_fixup(commit, window, cx),
                    cx,
                );
                return;
            }
            GraphEvent::CheckoutBranch(name) => {
                let op = BranchOp::Switch { name: name.clone() };
                self.with_window(
                    move |this, window, cx| this.run_branch_op(op, window, cx),
                    cx,
                );
                return;
            }
        };
        self.show_details(id, cx);
    }

    /// Shows `id`'s details in the right panel (instead of the working changes).
    /// The labels a commit has in the graph, and its lane color, for the details panel.
    fn commit_refs(&self, id: ObjectId) -> (Vec<RefLabel>, usize) {
        let RepoState::Loaded(repo) = &self.state else {
            return (Vec::new(), 0);
        };
        let graph = &repo.graph;
        let refs = graph.labels.get(&id).cloned().unwrap_or_default();
        let lane = graph
            .row_of(&id)
            .map_or(0, |row| usize::from(graph.layout.node(row).color));
        (refs, lane)
    }

    /// Reads the diff of the file the details panel previews, in its own lane: a newer
    /// selection supersedes it.
    fn load_preview(
        &mut self,
        commit: ObjectId,
        parent: Option<ObjectId>,
        change: gitweft_git::read::FileChange,
        cx: &mut Context<Self>,
    ) {
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let git = repo.git.clone();
        let cwd = repo.cwd();
        let options = DiffOptions {
            ignore_whitespace: self.diff_ignore_whitespace,
            ..DiffOptions::default()
        };
        let shown = change.path.clone();
        // Its syntax colors are computed on the job too, as for the diff view.
        let dark = cx.palette().dark;
        let tab = crate::code_scroll::tab_width(cx);
        let job = self
            .jobs
            .spawn(Lane::Latest("details-preview"), move |cancel| {
                let source = DiffSource::Commit { parent, commit };
                let path = change.path.clone();
                let old_path = change.old_path.clone();
                file_diff(
                    &git,
                    &cwd,
                    &source,
                    &path,
                    old_path.as_ref(),
                    options,
                    cancel,
                )
                .map(|diff| {
                    let syntax = decorate(&diff, &[], dark, tab).syntax;
                    (diff, syntax, tab)
                })
                .map_err(|err| match err.downcast_ref::<GitError>() {
                    // Git refuses the folder: say so rather than show its message.
                    Some(refused) if refused.kind == GitErrorKind::DubiousOwnership => {
                        t!("repair.untrusted_preview").into_owned()
                    }
                    _ => err.to_string(),
                })
            });
        let details = self.details_view.downgrade();
        cx.spawn(async move |_, cx| {
            if let Some(result) = job.join().await {
                details
                    .update(cx, |details, cx| {
                        details.set_preview(commit, &shown, result, cx)
                    })
                    .ok();
            }
        })
        .detach();
    }

    /// The details panel's ×: the working changes when there are some, else no selection.
    fn close_details(&mut self, cx: &mut Context<Self>) {
        let Some(graph) = self.graph_view.clone() else {
            return;
        };
        if !graph.update(cx, |graph, cx| graph.deselect(cx)) {
            self.details_view
                .update(cx, |details, cx| details.set_state(DetailsState::Empty, cx));
        }
    }

    fn show_details(&mut self, id: ObjectId, cx: &mut Context<Self>) {
        if self.showing_wip {
            self.showing_wip = false;
            cx.notify();
        }
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let (git, git_dir) = (repo.git.clone(), repo.repo.git_dir().to_owned());
        // The previous details stay visible until the new ones arrive (a few ms), so
        // arrowing through the graph does not flicker.
        let job = self
            .jobs
            .spawn(Lane::Latest("commit-details"), move |cancel| {
                commit_details(&git, &git_dir, id, cancel).map_err(|e| e.to_string())
            });
        cx.spawn(async move |this, cx| {
            let Some(result) = job.join().await else {
                return; // superseded by a newer selection
            };
            this.update(cx, |this, cx| {
                let state = match result {
                    Ok(details) => {
                        let (refs, lane) = this.commit_refs(details.id);
                        DetailsState::Loaded(Box::new(LoadedCommit {
                            details,
                            refs,
                            lane,
                        }))
                    }
                    Err(message) => DetailsState::Failed(message),
                };
                // The user may have moved to the working changes meanwhile.
                if !this.showing_wip {
                    this.details_view
                        .update(cx, |details, cx| details.set_state(state, cx));
                }
            })
            .ok();
        })
        .detach();
    }

    /// Runs a staging action in the write lane, then refreshes the status.
    fn on_staging_event(
        &mut self,
        _: Entity<StagingView>,
        event: &StagingEvent,
        cx: &mut Context<Self>,
    ) {
        if let StagingEvent::Ignore { path, choice } = event {
            let (path, choice) = (path.clone(), *choice);
            self.with_window(
                move |this, window, cx| this.add_ignore(path, choice, window, cx),
                cx,
            );
            return;
        }
        if let StagingEvent::RowMenu {
            entry,
            staged,
            selection,
        } = event
        {
            let (entry, staged, selection) = (entry.clone(), *staged, selection.clone());
            self.with_window(
                move |this, window, cx| this.open_file_menu(entry, staged, selection, window, cx),
                cx,
            );
            return;
        }
        if let StagingEvent::Stash(paths) = event {
            let paths = paths.clone();
            self.with_window(
                move |this, window, cx| this.open_stash_dialog(paths, window, cx),
                cx,
            );
            return;
        }
        if let StagingEvent::EditIdentity = event {
            self.with_window(
                |this, window, cx| this.open_identity_dialog(None, window, cx),
                cx,
            );
            return;
        }
        if let StagingEvent::External { entry, tool } = event {
            let (entry, tool) = (entry.clone(), *tool);
            self.with_window(
                move |this, window, cx| this.open_file_tool(entry, tool, window, cx),
                cx,
            );
            return;
        }
        match event {
            StagingEvent::OpenDiff { entry, staged } => {
                self.open_status_diff(entry, *staged, cx);
                // The list keeps the keys: they go on from the clicked file.
                if let Some(view) = &self.diff_view {
                    view.update(cx, |view, _| view.set_focus_on_open(false));
                }
                return;
            }
            StagingEvent::CloseDiff => {
                if self.diff_shows_worktree() {
                    self.close_diff();
                    cx.notify();
                }
                return;
            }
            StagingEvent::Commit { message, options } => {
                let (message, options) = (message.clone(), *options);
                self.with_window(
                    move |this, window, cx| this.commit_checked(message, options, window, cx),
                    cx,
                );
                return;
            }
            StagingEvent::Blame(path) => {
                self.open_blame(path.clone(), None, cx);
                return;
            }
            StagingEvent::History(path) => {
                self.open_history(path.clone(), None, cx);
                return;
            }
            _ => {}
        }
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let Some(workdir) = repo.workdir.clone() else {
            return;
        };
        let git = repo.git.clone();
        let unborn = self
            .status
            .as_ref()
            .is_some_and(|status| status.branch.oid.is_none());

        type Action =
            Box<dyn FnOnce(&CancelToken) -> Result<Option<DiscardBackup>, GitError> + Send>;
        let action: Action = match event {
            StagingEvent::Stage(entries) => {
                let paths = entries.iter().map(|e| e.path.clone()).collect::<Vec<_>>();
                Box::new(move |cancel| stage_paths(&git, &workdir, &paths, cancel).map(|()| None))
            }
            StagingEvent::Unstage(entries) => {
                let paths = with_rename_sources(entries);
                Box::new(move |cancel| {
                    unstage_paths(&git, &workdir, &paths, unborn, cancel).map(|()| None)
                })
            }
            StagingEvent::Discard(entries) => {
                let (untracked, tracked): (Vec<&StatusEntry>, Vec<&StatusEntry>) = entries
                    .iter()
                    .filter(|e| e.kind != EntryKind::Conflicted)
                    .partition(|e| e.kind == EntryKind::Untracked);
                let tracked: Vec<BString> = tracked.iter().map(|e| e.path.clone()).collect();
                let untracked: Vec<BString> = untracked.iter().map(|e| e.path.clone()).collect();
                Box::new(move |cancel| {
                    discard_paths(&git, &workdir, &tracked, &untracked, &trash_dir(), cancel)
                        .map(Some)
                })
            }
            // Handled above.
            StagingEvent::OpenDiff { .. }
            | StagingEvent::Commit { .. }
            | StagingEvent::Blame(_)
            | StagingEvent::History(_)
            | StagingEvent::Stash(_)
            | StagingEvent::Ignore { .. }
            | StagingEvent::External { .. }
            | StagingEvent::EditIdentity
            | StagingEvent::CloseDiff
            | StagingEvent::RowMenu { .. } => return,
        };

        let job = self.jobs.spawn(Lane::Write, move |cancel| action(cancel));
        cx.spawn(async move |this, cx| {
            let result = job.join().await;
            this.update(cx, |this, cx| {
                match result {
                    Some(Ok(_)) | None => {}
                    Some(Err(err)) => this.with_window(
                        move |this, window, cx| {
                            let title = t!("repair.staging_failed").into_owned();
                            this.show_git_error(title, &err, window, cx)
                        },
                        cx,
                    ),
                }
                this.refresh_status(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Commits the index (write lane). Hook output of a failed commit goes back to the
    /// commit box; after a successful one the graph reloads.
    fn run_commit(&mut self, message: String, options: CommitOptions, cx: &mut Context<Self>) {
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let Some(workdir) = repo.workdir.clone() else {
            return;
        };
        let git = repo.git.clone();
        let summary = message.lines().next().unwrap_or_default().trim().to_owned();
        // A commit that finishes a merge (or another operation) is undone like a merge:
        // the files go back too. Otherwise its changes come back staged.
        let worktree = if self.operation.is_some() {
            Worktree::Follow
        } else {
            Worktree::Soft
        };
        let record = Record::checked_out(
            t!("undo.op_commit", summary = summary).into_owned(),
            worktree,
        );
        let retry = (message.clone(), options);
        let job = self.jobs.spawn(Lane::Write, move |cancel| {
            recorded(&git, &workdir, Some(record), cancel, || {
                gitweft_git::ops::commit(&git, &workdir, &message, options, cancel)
            })
        });
        cx.spawn(async move |this, cx| {
            let result = job.join().await.unwrap_or_else(|| {
                let text = t!("commit.cancelled").into_owned();
                Err(GitError::new(GitErrorKind::Cancelled, "git commit", text))
            });
            this.update(cx, |this, cx| {
                let result = this.take_entry(result, cx);
                let committed = result.is_ok();
                // Refusals with a way out offer it; without an identity, the commit runs
                // again once one is saved.
                if let Err(err) = &result {
                    match &err.kind {
                        GitErrorKind::IdentityUnknown => this.with_window(
                            move |this, window, cx| {
                                this.open_identity_dialog(Some(retry), window, cx)
                            },
                            cx,
                        ),
                        GitErrorKind::LockHeld { .. } | GitErrorKind::DubiousOwnership => {
                            let err = err.clone();
                            this.with_window(
                                move |this, window, cx| {
                                    this.show_git_error(String::new(), &err, window, cx)
                                },
                                cx,
                            )
                        }
                        _ => {}
                    }
                }
                // Hooks explain themselves on stderr; show that, not just the last line.
                let result = result.map(|_| ()).map_err(|e| {
                    if e.stderr.trim().is_empty() {
                        e.summary()
                    } else {
                        e.stderr.trim().to_owned()
                    }
                });
                this.staging_view
                    .update(cx, |staging, cx| staging.commit_finished(result, cx));
                if committed {
                    this.reload_after_commit(cx);
                } else {
                    this.refresh_status(cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Writes the commit-graph file, then reloads the repository to benefit from it.
    fn write_commit_graph(&mut self, cx: &mut Context<Self>) {
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let (git, git_dir) = (repo.git.clone(), repo.repo.git_dir().to_owned());
        self.commit_graph_banner = CommitGraphBanner::Writing;
        cx.notify();
        let job = self.jobs.spawn(Lane::Write, move |cancel| {
            write_commit_graph(&git, &git_dir, cancel)
        });
        cx.spawn(async move |this, cx| {
            let result = job.join().await;
            this.update(cx, |this, cx| {
                this.commit_graph_banner = CommitGraphBanner::Hidden;
                match result {
                    Some(Ok(())) => {
                        if let Some(path) = this.current_path.clone() {
                            this.open(path, cx);
                        }
                    }
                    Some(Err(err)) => {
                        let text = t!("commit_graph.failed", error = err.to_string()).into_owned();
                        crate::tab_notes::note_failure(text, cx);
                    }
                    None => {}
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn render_commit_graph_banner(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let look = cx.look().clone();
        let writing = self.commit_graph_banner == CommitGraphBanner::Writing;
        let message = if writing {
            t!("graph.commit_graph_writing")
        } else {
            t!("graph.commit_graph_hint")
        }
        .into_owned();
        let buttons = if writing {
            Vec::new()
        } else {
            vec![
                Button::new("write-commit-graph")
                    .small()
                    .label(t!("graph.commit_graph_write").into_owned())
                    .on_click(cx.listener(|this, _, _, cx| this.write_commit_graph(cx)))
                    .into_any_element(),
                Button::new("dismiss-commit-graph")
                    .ghost()
                    .small()
                    .label(t!("graph.commit_graph_dismiss").into_owned())
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.commit_graph_banner = CommitGraphBanner::Hidden;
                        // Kept for the repository: reloads don't bring it back.
                        this.sync.settings.commit_graph_dismissed = true;
                        this.save_repo_settings(cx);
                        cx.notify();
                    }))
                    .into_any_element(),
            ]
        };
        widgets::banner(AppIcon::Info, look.palette.acc, message, buttons, &look)
    }

    /// A merge, rebase, cherry-pick or revert in progress (possibly started in a terminal):
    /// what it is, how many conflicts are left, and the ways forward.
    fn render_operation_banner(
        &self,
        operation: Operation,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let look = cx.look().clone();
        let conflicts = self
            .status
            .as_ref()
            .map_or(0, |status| status.conflicts().count());
        let name = operation_label(operation);
        let text = match conflicts {
            // Paused by an edit or a pause in the plan (or its conflicts are resolved).
            0 if operation == Operation::Rebase => t!("op.rebase_paused"),
            0 => t!("op.in_progress", op = name),
            count => tn!("op.in_progress_conflicts", count = count, op = name),
        }
        .into_owned();
        let busy = self.remote_op.is_some();
        let abort = operation_button(
            "op-abort",
            t!("op.abort").into_owned(),
            operation,
            OperationAction::Abort,
            false,
            busy,
            cx,
        );
        let skip = operation.can_skip().then(|| {
            operation_button(
                "op-skip",
                t!("op.skip").into_owned(),
                operation,
                OperationAction::Skip,
                false,
                busy,
                cx,
            )
        });
        // Continuing needs every file resolved, as in the conflict header.
        let proceed = operation.can_continue().then(|| {
            operation_button(
                "op-continue",
                t!("op.continue").into_owned(),
                operation,
                OperationAction::Continue,
                conflicts == 0,
                busy || conflicts > 0,
                cx,
            )
            .when(conflicts > 0, |button| {
                button.tooltip(t!("op.continue_unresolved").into_owned())
            })
        });
        let mut buttons = vec![abort.into_any_element()];
        buttons.extend(skip.map(IntoElement::into_any_element));
        buttons.extend(proceed.map(IntoElement::into_any_element));
        widgets::banner(AppIcon::Merge, look.palette.modified, text, buttons, &look)
    }

    /// Opens the conflict tool for a conflicted file (the file is read in the background).
    fn open_conflict(&mut self, path: BString, window: &mut Window, cx: &mut Context<Self>) {
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let Some(workdir) = repo.workdir.clone() else {
            return;
        };
        let file = workdir.join(gitweft_platform::bytes::path_from_bytes(&path));
        /// The line-by-line tool's text, or what each side has for a whole-file conflict
        /// (and the incoming commit, to name that side: the file has no markers).
        enum Opened {
            /// The text, and the size of its conflict markers (`conflict-marker-size`).
            Text(String, usize),
            // Boxed: the sides hold object ids, 32 bytes each with SHA-256 enabled.
            Whole(Box<conflict_files::ConflictSides>, Option<ObjectId>),
        }
        let git = repo.git.clone();
        let git_dir = repo.repo.git_dir().to_owned();
        let job_path = path.clone();
        let job = self
            .jobs
            .spawn(Lane::Latest("conflict-file"), move |cancel| {
                let sides = conflict_files::conflict_sides(
                    &git,
                    &workdir,
                    gitweft_git::gix::bstr::BStr::new(job_path.as_slice()),
                    cancel,
                )
                .unwrap_or_default();
                let whole = |sides| Opened::Whole(Box::new(sides), incoming_commit(&git_dir));
                if sides.whole_file() {
                    return whole(sides);
                }
                // Binary or not UTF-8: sides whole too.
                match std::fs::read(&file) {
                    Ok(bytes) if !conflict_panel::looks_binary(&bytes) => {
                        match String::from_utf8(bytes) {
                            Ok(text) => {
                                let size = conflict_files::conflict_marker_size(
                                    &git, &workdir, &job_path, cancel,
                                );
                                Opened::Text(text, size)
                            }
                            Err(_) => whole(sides),
                        }
                    }
                    _ => whole(sides),
                }
            });
        cx.spawn_in(window, async move |this, cx| {
            let Some(opened) = job.join().await else {
                return;
            };
            this.update_in(cx, |this, window, cx| {
                let view = match opened {
                    Opened::Text(text, size) => {
                        cx.new(|cx| ConflictView::new(path, text, size, window, cx))
                    }
                    Opened::Whole(sides, incoming) => {
                        let theirs = incoming.map(|oid| this.commit_name(oid));
                        cx.new(|cx| ConflictView::whole_file(path, *sides, theirs, window, cx))
                    }
                };
                this._conflict_events =
                    Some(cx.subscribe_in(&view, window, Self::on_conflict_event));
                this.conflict_view = Some(view);
                this.close_diff();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// A commit's name for the conflict header: a branch pointing at it (local first), else
    /// its short SHA.
    fn commit_name(&self, oid: ObjectId) -> String {
        let RepoState::Loaded(repo) = &self.state else {
            return oid.to_hex_with_len(8).to_string();
        };
        repo.refs
            .local_branches
            .iter()
            .chain(&repo.refs.remote_branches)
            .find(|entry| entry.target == oid)
            .map_or_else(
                || oid.to_hex_with_len(8).to_string(),
                |entry| entry.short_name.clone(),
            )
    }

    /// Resolves a whole-file conflict with one side's version (or its deletion), then
    /// closes the tool.
    fn take_conflict_side(
        &mut self,
        path: BString,
        side: conflict_files::Side,
        deletes: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let shown = path.to_string();
        let message = match (deletes, side) {
            (true, _) => t!("conflict.took_deletion", path = shown),
            (false, conflict_files::Side::Ours) => t!("conflict.took_ours", path = shown),
            (false, conflict_files::Side::Theirs) => t!("conflict.took_theirs", path = shown),
        }
        .into_owned();
        self.conflict_view = None;
        self._conflict_events = None;
        self.run_write_op(
            t!("conflict.take_failed").into_owned(),
            None,
            move |git, cwd, cancel| {
                let path = gitweft_git::gix::bstr::BStr::new(path.as_slice());
                conflict_files::take_side(git, cwd, path, side, cancel).map(|()| message)
            },
            window,
            cx,
        );
        cx.notify();
    }

    fn on_conflict_event(
        &mut self,
        _: &Entity<ConflictView>,
        event: &ConflictEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            ConflictEvent::Close => {
                self.conflict_view = None;
                self._conflict_events = None;
                cx.notify();
            }
            ConflictEvent::Save {
                path,
                content,
                mark_resolved,
            } => self.save_conflict(path.clone(), content.clone(), *mark_resolved, window, cx),
            ConflictEvent::TakeSide {
                path,
                side,
                deletes,
            } => self.take_conflict_side(path.clone(), *side, *deletes, window, cx),
            ConflictEvent::MergeTool(path) => self.run_merge_tool(path.clone(), window, cx),
        }
    }

    /// Writes the resolved file and, with `mark_resolved`, stages it. The tool closes
    /// only once that worked, so a failed write loses nothing.
    fn save_conflict(
        &mut self,
        path: BString,
        content: String,
        mark_resolved: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let Some(workdir) = repo.workdir.clone() else {
            return;
        };
        let git = repo.git.clone();
        let shown = path.to_string();
        let job = self
            .jobs
            .spawn(Lane::Write, move |cancel| -> Result<String, GitError> {
                let file = workdir.join(gitweft_platform::bytes::path_from_bytes(&path));
                std::fs::write(&file, content.as_bytes())
                    .map_err(|err| GitError::new(GitErrorKind::Other, "write", err.to_string()))?;
                if mark_resolved {
                    stage_paths(&git, &workdir, std::slice::from_ref(&path), cancel)?;
                    Ok(t!("conflict.resolved", path = shown).into_owned())
                } else {
                    Ok(t!("conflict.saved", path = shown).into_owned())
                }
            });
        cx.spawn_in(window, async move |this, cx| {
            let result = job.join().await;
            this.update_in(cx, |this, _, cx| {
                match result {
                    Some(Ok(message)) => {
                        note(Toast::success(message), cx);
                        if mark_resolved {
                            this.conflict_view = None;
                            this._conflict_events = None;
                        }
                    }
                    Some(Err(err)) => note(
                        Toast::error(err.summary()).title(t!("conflict.failed").into_owned()),
                        cx,
                    ),
                    None => {}
                }
                this.refresh_status(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Continues, skips or aborts the operation in progress (write lane).
    fn run_operation(
        &mut self,
        operation: Operation,
        action: OperationAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Aborting throws this step's conflict work away, and undo can't bring it back: ask
        // first when the step stopped on conflicted files.
        let files = self.conflict_panel.file_count();
        if action == OperationAction::Abort && files > 0 {
            self.confirm_abort(operation, files, window, cx);
            return;
        }
        self.run_operation_now(operation, action, window, cx);
    }

    /// Asks before aborting `operation`, stopped on `files` conflicted files (some of them
    /// resolved, perhaps).
    fn confirm_abort(
        &mut self,
        operation: Operation,
        files: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let unresolved = self
            .status
            .as_ref()
            .map_or(0, |status| status.conflicts().count());
        let resolved = files.saturating_sub(unresolved);
        let message = t!("op.abort_confirm", op = operation_label(operation)).into_owned();
        let detail = if resolved == 0 {
            t!("op.abort_confirm_none").into_owned()
        } else {
            tn!("op.abort_confirm_resolved", count = resolved, total = files).into_owned()
        };
        self.confirm(
            message,
            detail,
            t!("op.abort").into_owned(),
            move |this, confirmed, window, cx| {
                if confirmed {
                    this.run_operation_now(operation, OperationAction::Abort, window, cx)
                }
            },
            window,
            cx,
        );
    }

    fn run_operation_now(
        &mut self,
        operation: Operation,
        action: OperationAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A merge is committed with the panel's message (git's, as the user edited it).
        if (operation, action) == (Operation::Merge, OperationAction::Continue)
            && self.conflict_panel.has_message(cx)
        {
            self.commit_merge(window, cx);
            return;
        }
        let name = operation_label(operation);
        let done = match action {
            OperationAction::Continue => t!("op.continued", op = name),
            OperationAction::Skip => t!("op.skipped", op = name),
            OperationAction::Abort => t!("op.aborted", op = name),
        }
        .into_owned();
        // A rebase or cherry-pick sequence can stop at the next conflict.
        let conflict = Some((
            t!("op.next_conflict_title").into_owned(),
            t!("op.next_conflict_text").into_owned(),
        ));
        if (operation, action) == (Operation::Rebase, OperationAction::Continue) {
            let failed = t!("op.failed", op = name).into_owned();
            self.continue_rebase(failed, conflict, done, window, cx);
            return;
        }
        // Continuing a merge, cherry-pick or revert makes a commit, which undo takes back
        // (commits a sequence made before it stopped aren't part of it).
        let record = match (operation, action) {
            (Operation::Merge, OperationAction::Continue) => Some(t!("undo.op_merge_commit")),
            (Operation::CherryPick, OperationAction::Continue) => {
                Some(t!("undo.op_cherry_pick_commit"))
            }
            (Operation::Revert, OperationAction::Continue) => Some(t!("undo.op_revert_commit")),
            _ => None,
        }
        .map(|label| Record::checked_out(label.into_owned(), Worktree::Follow));
        self.run_recorded_op(
            t!("op.failed", op = name).into_owned(),
            conflict,
            record,
            move |git, cwd, cancel| {
                run_operation_action(git, cwd, operation, action, cancel)?;
                Ok(done)
            },
            window,
            cx,
        );
    }

    /// "Merge into current branch…": previews the merge (in memory), then asks, naming
    /// the files that would conflict.
    fn merge_into_current(
        &mut self,
        revision: String,
        shown: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let head = head_label(&repo.refs.head);
        self.merge_into(revision, shown, head, window, cx);
    }

    /// [`Self::merge_into_current`] naming the checked-out branch `head` (right after a
    /// switch, the refs read last still name the one before).
    fn merge_into(
        &mut self,
        revision: String,
        shown: String,
        head: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let git = repo.git.clone();
        let cwd = repo.cwd();
        // The short name when it's unambiguous, so git's message names the branch plainly.
        let names = |entries: &[RefEntry]| -> Vec<String> {
            entries
                .iter()
                .map(|entry| entry.short_name.clone())
                .collect()
        };
        let (tags, locals) = (names(&repo.refs.tags), names(&repo.refs.local_branches));
        // A branch whose name isn't UTF-8 goes by its full name, encoded (`git_arg` gives
        // git its bytes): the text shown names nothing to git.
        let encoded = repo
            .refs
            .local_branches
            .iter()
            .find(|entry| {
                std::str::from_utf8(&entry.full_name).is_err()
                    && String::from_utf8_lossy(&entry.full_name) == revision.as_str()
            })
            .map(|entry| gitweft_git::undo::ref_name_text(&entry.full_name));
        let revision = encoded.unwrap_or_else(|| {
            merge_revision(
                &revision,
                &tags.iter().map(String::as_str).collect::<Vec<_>>(),
                &locals.iter().map(String::as_str).collect::<Vec<_>>(),
            )
        });
        let preview_revision = revision.clone();
        let job = self
            .jobs
            .spawn(Lane::Latest("merge-preview"), move |cancel| {
                merge_preview(&git, &cwd, "HEAD", Self::git_arg(&preview_revision), cancel)
            });
        cx.spawn_in(window, async move |this, cx| {
            let Some(preview) = job.join().await else {
                return;
            };
            this.update_in(cx, |this, window, cx| {
                let text = match &preview {
                    Ok(preview) if preview.is_clean() => t!("merge.clean").into_owned(),
                    Ok(preview) => {
                        let mut files: Vec<String> = preview
                            .conflicts
                            .iter()
                            .take(8)
                            .map(|path| String::from_utf8_lossy(path).into_owned())
                            .collect();
                        if preview.conflicts.len() > 8 {
                            files.push("…".to_owned());
                        }
                        let files = crate::util::join_list(&files);
                        match preview.conflicts.len() {
                            1 => t!("merge.conflict", files = files).into_owned(),
                            count => {
                                tn!("merge.conflicts", count = count, files = files).into_owned()
                            }
                        }
                    }
                    // No preview (e.g. unrelated histories): git says more when merging.
                    Err(err) => err.summary(),
                };
                let (merge_revision, branch) = (revision.clone(), shown.clone());
                let run: ChoiceAction = Rc::new(move |this, window, cx| {
                    this.run_merge(merge_revision.clone(), branch.clone(), window, cx)
                });
                this.open_choice(
                    t!("merge.title", branch = shown, head = head).into_owned(),
                    text,
                    t!("toolbar.cancel").into_owned(),
                    vec![(t!("merge.button").into_owned(), run)],
                    window,
                    cx,
                );
            })
            .ok();
        })
        .detach();
    }

    fn run_merge(
        &mut self,
        revision: String,
        shown: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let conflict = Some((
            t!("merge.conflicted_title").into_owned(),
            t!("merge.conflicted_text").into_owned(),
        ));
        let record = Record::checked_out(
            t!("undo.op_merge", branch = shown).into_owned(),
            Worktree::Follow,
        );
        self.run_recorded_op(
            t!("merge.failed").into_owned(),
            conflict,
            Some(record),
            move |git, cwd, cancel| {
                let outcome = merge(
                    git,
                    cwd,
                    Self::git_arg(&revision),
                    MergeMode::Default,
                    cancel,
                )?;
                Ok(match outcome {
                    MergeOutcome::UpToDate => t!("merge.up_to_date"),
                    MergeOutcome::FastForwarded => t!("merge.fast_forwarded", branch = shown),
                    MergeOutcome::Merged => t!("merge.merged", branch = shown),
                    MergeOutcome::Squashed => t!("merge.squashed"),
                }
                .into_owned())
            },
            window,
            cx,
        );
    }

    fn render_watch_banner(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let look = cx.look().clone();
        let dismiss = Button::new("dismiss-watch-limited")
            .ghost()
            .small()
            .label(t!("watch.dismiss").into_owned())
            .on_click(cx.listener(|this, _, _, cx| {
                this.watch_limited = false;
                cx.notify();
            }))
            .into_any_element();
        widgets::banner(
            AppIcon::Info,
            look.palette.modified,
            if self.watch_stopped {
                t!("watch.stopped")
            } else {
                t!("watch.limited")
            }
            .into_owned(),
            vec![dismiss],
            &look,
        )
    }

    /// Queues work that needs the window (see [`WindowTask`]).
    fn with_window(
        &mut self,
        task: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) {
        self.window_tasks.push(Box::new(task));
        cx.notify();
    }

    /// Reads upstream and ahead/behind for every local branch (sidebar badges).
    fn load_tracking(&mut self, cx: &mut Context<Self>) {
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let git = repo.git.clone();
        let cwd = repo.cwd();
        let job = self.jobs.spawn(Lane::Coalesce("tracking"), move |cancel| {
            branch_tracking(&git, &cwd, cancel).unwrap_or_default()
        });
        cx.spawn(async move |this, cx| {
            if let Some(list) = job.join().await {
                this.update(cx, |this, cx| {
                    this.tracking = list.into_iter().map(|t| (t.branch.clone(), t)).collect();
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    /// Runs a local write (stash, tag) in the write lane. Success shows a notification; a
    /// conflict is explained in a dialog (`conflict`: title, text); anything else shows
    /// git's message under `failed_title`.
    fn run_write_op(
        &mut self,
        failed_title: String,
        conflict: Option<(String, String)>,
        job: impl FnOnce(&GitCli, &Path, &CancelToken) -> Result<String, GitError> + Send + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.run_recorded_op(failed_title, conflict, None, job, window, cx)
    }

    /// [`Self::run_write_op`] that also records the refs named in `record` for undo.
    fn run_recorded_op(
        &mut self,
        failed_title: String,
        conflict: Option<(String, String)>,
        record: Option<Record>,
        job: impl FnOnce(&GitCli, &Path, &CancelToken) -> Result<String, GitError> + Send + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.run_entry_op(
            failed_title,
            conflict,
            move |git, cwd, cancel| recorded(git, cwd, record, cancel, || job(git, cwd, cancel)),
            window,
            cx,
        )
    }

    /// [`Self::run_write_op`] for a job that makes its own undo entry.
    fn run_entry_op(
        &mut self,
        failed_title: String,
        conflict: Option<(String, String)>,
        job: impl FnOnce(&GitCli, &Path, &CancelToken) -> Result<(String, Option<UndoEntry>), GitError>
        + Send
        + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let git = repo.git.clone();
        let cwd = repo.cwd();
        let task = self
            .jobs
            .spawn(Lane::Write, move |cancel| job(&git, &cwd, cancel));
        cx.spawn_in(window, async move |this, cx| {
            let result = task.join().await;
            this.update_in(cx, |this, window, cx| {
                match result.map(|result| this.take_entry(result, cx)) {
                    Some(Ok(message)) => note(Toast::success(message), cx),
                    Some(Err(err)) => match (&conflict, &err.kind) {
                        (Some((title, text)), GitErrorKind::Conflict) => this.open_choice(
                            title.clone(),
                            text.clone(),
                            t!("branch.ok").into_owned(),
                            Vec::new(),
                            window,
                            cx,
                        ),
                        _ => this.show_git_error(failed_title, &err, window, cx),
                    },
                    None => {}
                }
                this.refresh_after_write(cx);
            })
            .ok();
        })
        .detach();
    }

    /// What a finished write refreshes: refs, status, stashes, remotes, worktrees and
    /// submodules.
    fn refresh_after_write(&mut self, cx: &mut Context<Self>) {
        self.refresh_refs(cx);
        self.refresh_status(cx);
        self.load_stashes(cx);
        self.load_remotes(cx);
        self.load_worktrees(cx);
        self.load_submodules(cx);
        cx.notify();
    }

    /// Keeps the undo entry of a finished write, if it recorded one; returns its value.
    fn take_entry<T, E>(
        &mut self,
        result: Result<(T, Option<UndoEntry>), E>,
        cx: &mut Context<Self>,
    ) -> Result<T, E> {
        result.map(|(value, entry)| {
            if let Some(entry) = entry {
                self.push_undo(entry, cx);
            }
            value
        })
    }

    /// Reads the submodules for the sidebar, the nested ones of those set up included.
    fn load_submodules(&mut self, cx: &mut Context<Self>) {
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let git = repo.git.clone();
        let Some(cwd) = repo.workdir.clone() else {
            return;
        };
        let job = self
            .jobs
            .spawn(Lane::Coalesce("submodules"), move |cancel| {
                list_submodules_nested(&git, &cwd, cancel).unwrap_or_default()
            });
        cx.spawn(async move |this, cx| {
            if let Some(submodules) = job.join().await {
                this.update(cx, |this, cx| {
                    if this.submodules != submodules {
                        this.submodules = submodules;
                        cx.notify();
                    }
                })
                .ok();
            }
        })
        .detach();
    }

    /// Checks out the recorded commits of `paths` inside the repository at `superproject`
    /// (all its submodules when empty), cloning the ones not set up yet, nested ones
    /// included. It may fetch, so it runs as a network operation: its progress and Cancel
    /// in the status bar. It changes other repositories, so it isn't undoable.
    pub(super) fn run_submodule_update(
        &mut self,
        superproject: PathBuf,
        paths: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.remote_op.is_some() {
            return;
        }
        self.submodule_update = Some((superproject, paths));
        self.run_remote(RemoteOpKind::SubmoduleUpdate, window, cx);
    }

    /// Copies a submodule's URL from `.gitmodules` into the configuration of the
    /// repository at `superproject` (and its checkout's `origin`).
    fn run_submodule_sync(
        &mut self,
        superproject: PathBuf,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.run_write_op(
            t!("submodule.failed").into_owned(),
            None,
            move |git, cwd, cancel| {
                let folder = cwd.join(&superproject);
                sync_submodules(git, &folder, std::slice::from_ref(&path), true, cancel)?;
                let name = superproject.join(&path);
                Ok(t!("submodule.synced", name = name.display()).into_owned())
            },
            window,
            cx,
        );
    }

    fn render_repo(&self, repo: &LoadedRepo, cx: &mut Context<Self>) -> impl IntoElement {
        let sidebar = self.render_sidebar(repo, cx).into_any_element();
        let activity_panel = self
            .show_activity
            .then(|| self.render_activity(cx).into_any_element());
        // An open diff takes the graph's place (the graph keeps its state).
        let centre = self.graph_view.clone().map(|graph| {
            let centre = if let Some(conflict) = &self.conflict_view {
                v_flex()
                    .size_full()
                    .child(self.render_conflict_header(conflict, cx))
                    .child(div().flex_1().min_h_0().child(conflict.clone()))
                    .into_any_element()
            } else if let Some(rebase) = &self.rebase_view {
                rebase.clone().into_any_element()
            } else if let Some(blame) = &self.blame_view {
                blame.clone().into_any_element()
            } else if let Some(diff) = &self.diff_view {
                diff.clone().into_any_element()
            } else if let Some(file) = &self.file_view {
                file.clone().into_any_element()
            } else if let Some(history) = &self.history_view {
                history.clone().into_any_element()
            } else if let Some(tree) = &self.tree_view {
                tree.clone().into_any_element()
            } else if let Some(reflog) = &self.reflog_view {
                reflog.clone().into_any_element()
            } else if let Some(dangling) = &self.dangling_view {
                dangling.clone().into_any_element()
            } else if let Some(trash) = &self.trash_view {
                trash.clone().into_any_element()
            } else if let Some(compare) = &self.compare_view {
                compare.clone().into_any_element()
            } else {
                graph.into_any_element()
            };
            v_flex()
                .size_full()
                .child(div().flex_1().min_h_0().child(centre))
                .children(activity_panel)
                .into_any_element()
        });
        let right = if self.showing_wip {
            self.render_working_panel(cx)
        } else {
            self.details_view.clone().into_any_element()
        };
        self.render_panels(sidebar, centre, right, cx)
    }
}

/// Paths to unstage: a staged rename also needs its source path restored.
fn with_rename_sources(entries: &[StatusEntry]) -> Vec<BString> {
    entries
        .iter()
        .flat_map(|e| std::iter::once(e.path.clone()).chain(e.orig_path.clone()))
        .collect()
}

/// "↑2 ↓1" for a branch that differs from its upstream, "gone" when the upstream was
/// deleted; `None` when it is in sync (or has no upstream).
/// A local branch's name as `Tracking` has it, from its full name's bytes.
pub(super) fn branch_key(full_name: &[u8]) -> String {
    gitweft_git::undo::ref_name_text(full_name.strip_prefix(b"refs/heads/").unwrap_or(full_name))
}

fn tracking_badge(tracking: &Tracking, muted: Hsla, warning: Hsla) -> Option<(String, Hsla)> {
    if tracking.gone {
        return Some((t!("branch.upstream_gone").into_owned(), warning));
    }
    let parts: Vec<String> = [(tracking.ahead, '↑'), (tracking.behind, '↓')]
        .into_iter()
        .filter(|(count, _)| *count > 0)
        .map(|(count, arrow)| format!("{arrow}{count}"))
        .collect();
    (!parts.is_empty()).then(|| (parts.join(" "), muted))
}

/// The name of an operation for banners and notifications.
fn operation_label(operation: Operation) -> String {
    match operation {
        Operation::Merge => t!("op.merge"),
        Operation::Rebase => t!("op.rebase"),
        Operation::CherryPick => t!("op.cherry_pick"),
        Operation::Revert => t!("op.revert"),
        Operation::ApplyMailbox => t!("op.am"),
        Operation::Bisect => t!("op.bisect"),
    }
    .into_owned()
}

/// A button of the operation banner.
fn operation_button(
    id: &'static str,
    label: String,
    operation: Operation,
    action: OperationAction,
    primary: bool,
    disabled: bool,
    cx: &mut Context<Workspace>,
) -> Button {
    Button::new(id)
        .small()
        .label(label)
        .when(primary, |button| button.primary())
        .when(!primary, |button| button.ghost())
        .disabled(disabled)
        .on_click(
            cx.listener(move |this, _, window, cx| {
                this.run_operation(operation, action, window, cx)
            }),
        )
}

/// A `git format-patch` file (a mailbox) starts with its `From <sha>` line.
fn is_mailbox(file: &Path) -> bool {
    use std::io::Read as _;
    let mut start = [0u8; 5];
    std::fs::File::open(file)
        .and_then(|mut opened| opened.read_exact(&mut start))
        .is_ok()
        && &start == b"From "
}

/// A worktree's folder name, for labels and messages.
fn folder_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// Where a new worktree goes when the user chose `folder`: the folder itself, unless it
/// has files already (git refuses those), then inside it under `default`'s name.
fn worktree_in(folder: &Path, default: &Path) -> PathBuf {
    let occupied = folder
        .read_dir()
        .is_ok_and(|mut entries| entries.next().is_some());
    match default.file_name() {
        Some(name) if occupied => folder.join(name),
        _ => folder.to_owned(),
    }
}

/// The local branches checked out in worktrees other than `current` (short name → the
/// worktree's folder name), for the graph's pills.
fn other_worktree_branches(
    worktrees: &[WorktreeInfo],
    current: Option<&Path>,
) -> HashMap<String, SharedString> {
    worktrees
        .iter()
        .filter(|info| Some(info.path.as_path()) != current)
        .filter_map(|info| {
            let branch = info.branch.as_deref()?.strip_prefix(b"refs/heads/")?;
            Some((
                String::from_utf8_lossy(branch).into_owned(),
                folder_name(&info.path).into(),
            ))
        })
        .collect()
}

/// A worktree in the sidebar: its folder and what it has checked out.
fn worktree_label(info: &WorktreeInfo) -> String {
    let what = match (&info.branch, info.head) {
        (Some(branch), _) => {
            String::from_utf8_lossy(branch.strip_prefix(b"refs/heads/").unwrap_or(branch))
                .into_owned()
        }
        (None, Some(head)) => t!("worktree.detached", sha = head.to_hex_with_len(8)).into_owned(),
        (None, None) => String::new(),
    };
    let folder = folder_name(&info.path);
    if what.is_empty() {
        folder
    } else {
        t!("common.middle_dot", first = folder, second = what).into_owned()
    }
}

/// Refuses a remote name git wouldn't take, or an empty URL, with a sentence saying so.
fn check_remote(
    git: &GitCli,
    cwd: &Path,
    name: &str,
    url: Option<&str>,
    cancel: &CancelToken,
) -> Result<(), GitError> {
    let refuse = |text: String| GitError::new(GitErrorKind::Other, "git remote", text);
    if !is_valid_remote_name(git, cwd, name, cancel) {
        return Err(refuse(t!("remote.invalid_name", name = name).into_owned()));
    }
    if url.is_some_and(str::is_empty) {
        return Err(refuse(t!("remote.url_required").into_owned()));
    }
    Ok(())
}

/// A stash's name for people: its message without git's `On main: ` prefix. One made
/// without a message is git's `WIP on main: 1a2b3c4 Subject`, a commit's short ID and
/// subject that read like the stash's own: it's "Changes on main, on top of “Subject”".
pub(crate) fn stash_label(entry: &StashEntry) -> String {
    let message = entry.message.as_str();
    if let Some((branch, base)) = message
        .strip_prefix("WIP on ")
        .and_then(|rest| rest.split_once(": "))
    {
        let subject = base.split_once(' ').map_or(base, |(_, subject)| subject);
        return t!("stash.unnamed", branch = branch, subject = subject).into_owned();
    }
    message
        .split_once(": ")
        .map_or(message, |(_, rest)| rest)
        .to_owned()
}

/// One git command in the activity panel: its outcome, time, command line and duration,
/// with a failure's summary under it.
fn activity_row(entry: ActivityEntry, p: &Palette, m: &Metrics) -> impl IntoElement {
    let z = |value: f32| px(value * m.zoom);
    let seconds = entry
        .started
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64);
    let (mark, color, failure) = match entry.outcome {
        ActivityOutcome::Succeeded => ("✓", p.add, None),
        ActivityOutcome::Failed { summary, .. } => ("✗", p.del, Some(summary)),
        ActivityOutcome::Cancelled => ("–", p.fg3, Some(t!("activity.cancelled").into_owned())),
    };
    v_flex()
        .py(z(2.))
        .child(
            h_flex()
                .gap(z(8.))
                .child(
                    div()
                        .w(z(12.))
                        .flex_shrink_0()
                        .text_color(color)
                        .child(mark),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .text_color(p.fg3)
                        .child(format_time(seconds)),
                )
                .child(
                    truncated("command", entry.command)
                        .flex_1()
                        .text_color(p.fg2),
                )
                .child(div().flex_shrink_0().text_color(p.fg3).child(
                    t!("activity.duration_ms", ms = entry.duration.as_millis()).into_owned(),
                )),
        )
        .when_some(failure, |row, message| {
            row.child(div().pl(z(20.)).text_color(color).child(message))
        })
}

/// Debug builds: `GITWEFT_DEBUG_DIFF_LAYOUT=split` starts diffs in the split layout, for
/// screenshot checks without mouse input.
fn initial_diff_layout(cx: &App) -> DiffLayout {
    #[cfg(debug_assertions)]
    if std::env::var_os("GITWEFT_DEBUG_DIFF_LAYOUT").is_some_and(|v| v == "split") {
        return DiffLayout::Split;
    }
    view_layout(SettingsStore::settings(cx).diff.layout)
}

/// The diff view's layout for the one in the settings.
pub(super) fn view_layout(layout: gitweft_core::settings::DiffLayout) -> DiffLayout {
    match layout {
        gitweft_core::settings::DiffLayout::Unified => DiffLayout::Unified,
        gitweft_core::settings::DiffLayout::Split => DiffLayout::Split,
    }
}

/// The settings' layout for the diff view's.
fn settings_layout(layout: DiffLayout) -> gitweft_core::settings::DiffLayout {
    match layout {
        DiffLayout::Unified => gitweft_core::settings::DiffLayout::Unified,
        DiffLayout::Split => gitweft_core::settings::DiffLayout::Split,
    }
}

/// Debug builds: `GITWEFT_DEBUG_REMOTE=fetch|pull|push` runs that operation once after the
/// repository opens, so its outcome (notification or dialog) can be checked in screenshots.
#[cfg(debug_assertions)]
fn debug_remote_kind() -> Option<RemoteOpKind> {
    use std::sync::atomic::{AtomicBool, Ordering};
    static DONE: AtomicBool = AtomicBool::new(false);
    let kind = match std::env::var("GITWEFT_DEBUG_REMOTE").ok()?.as_str() {
        "fetch" => RemoteOpKind::Fetch,
        "pull" => RemoteOpKind::Pull(PullMode::Configured),
        "push" => RemoteOpKind::Push(ForceMode::None),
        _ => return None,
    };
    (!DONE.swap(true, Ordering::Relaxed)).then_some(kind)
}

/// Debug builds: `GITWEFT_DEBUG_DIALOG=<name>` opens that dialog once after the repository
/// opens (screenshot checks; menus that lead to it need a right-click).
#[cfg(debug_assertions)]
fn debug_dialog_once(name: &str) -> bool {
    use std::sync::atomic::{AtomicBool, Ordering};
    static DONE: AtomicBool = AtomicBool::new(false);
    std::env::var("GITWEFT_DEBUG_DIALOG").is_ok_and(|value| value == name)
        && !DONE.swap(true, Ordering::Relaxed)
}

/// The commit an operation stopped while bringing in (a merge's, a cherry-pick's, a
/// revert's, or the rebased commit), from the files git keeps in `git_dir` meanwhile.
fn incoming_commit(git_dir: &Path) -> Option<ObjectId> {
    [
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "REBASE_HEAD",
    ]
    .iter()
    .find_map(|name| {
        let text = std::fs::read_to_string(git_dir.join(name)).ok()?;
        ObjectId::from_hex(text.lines().next()?.trim().as_bytes()).ok()
    })
}

fn head_oid(head: &HeadInfo) -> Option<gitweft_git::ObjectId> {
    match head {
        HeadInfo::Branch { oid, .. } | HeadInfo::Detached { oid } => Some(*oid),
        HeadInfo::Unborn { .. } => None,
    }
}

/// Re-anchors a review's comments against the compared files as they are now: the new
/// side at the compared ref, the old side at the merge base. A file that isn't there
/// has no lines, so its comments are outdated; a cancelled job changes nothing.
fn reanchor_comments(
    git: &GitCli,
    cwd: &Path,
    comparison: &gitweft_git::read::Comparison,
    session: &mut ReviewSession,
    cancel: &CancelToken,
) {
    for (path, side) in session.commented() {
        let rev = match side {
            review::Side::New => comparison.head,
            review::Side::Old => comparison.merge_base.unwrap_or(comparison.base),
        };
        let name = review::path_bytes(&path);
        let bytes = match gitweft_git::read::file_content(git, cwd, Some(rev), &name, cancel) {
            Ok(bytes) => bytes,
            Err(_) if cancel.is_cancelled() => return,
            Err(_) => Vec::new(),
        };
        let text = String::from_utf8_lossy(&bytes);
        let lines: Vec<&str> = text.lines().collect();
        session.reanchor(&path, side, &lines);
    }
}

/// The remote branches holding `published`, for a sentence ("origin/main, upstream/main").
fn remote_list(published: &Published) -> String {
    if published.remote_branches.is_empty() {
        t!("rewrite.a_remote").into_owned()
    } else {
        crate::util::join_list(&published.remote_branches)
    }
}

/// Why leaving `published` behind needs care, for a confirmation's detail.
fn published_text(published: &Published) -> String {
    let remotes = remote_list(published);
    // One counted message: "it" or "them" follows the count.
    tn!(
        "rewrite.published",
        count = published.count,
        remotes = remotes
    )
    .into_owned()
}

/// The width of a choice dialog whose footer holds buttons with these labels: enough for
/// them in one row (four buttons with long labels ran past the kit's 480 px dialog, the
/// first hanging out of its left edge), never narrower than the kit's width.
fn choice_width<'a>(labels: impl Iterator<Item = &'a str>, zoom: f32) -> Pixels {
    let (count, chars) = labels.fold((0usize, 0usize), |(count, chars), label| {
        (count + 1, chars + label.chars().count())
    });
    // About 7.5 px per character of a button's text, 36 px of padding and gap per
    // button, and the dialog's own padding.
    let needed = chars as f32 * 7.5 + count as f32 * 36. + 48.;
    px(needed.max(480.) * zoom)
}

fn head_label(head: &HeadInfo) -> String {
    match head {
        HeadInfo::Branch { name, .. } => name.clone(),
        HeadInfo::Unborn { name } => t!("head.unborn", branch = name).into_owned(),
        HeadInfo::Detached { oid } => {
            t!("head.detached", sha = oid.to_hex_with_len(8)).into_owned()
        }
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.trace_startup(cx);
        // A graph reload (a view menu or palette toggle) replaces the view that had the
        // keys; that view isn't drawn any more, so shortcuts (Ctrl+Shift+P) reached nothing.
        // The new graph takes them, as it does when nothing has them.
        let replaced = self
            .replaced_focus
            .take()
            .is_some_and(|old| old.is_focused(window));
        // The staging lists can stop being shown while they have the keys: the last change
        // discarded, committed or stashed leaves no working changes, so the panel shows the
        // checked-out commit instead. Their element isn't drawn any more, but the window
        // still points at it, so every shortcut reached nothing until a view was clicked.
        // The graph takes them, as it does after a reload.
        let staging_gone = !self.showing_wip
            && self
                .staging_view
                .read(cx)
                .focus_handle(cx)
                .contains_focused(window, cx);
        if (replaced || staging_gone || window.focused(cx).is_none())
            && let Some(view) = &self.graph_view
        {
            let handle = view.read(cx).focus_handle(cx);
            window.focus(&handle, cx);
        }
        self.sidebar_focused = self.sidebar_focus.contains_focused(window, cx);
        for task in std::mem::take(&mut self.window_tasks) {
            cx.defer_in(window, move |this, window, cx| task(this, window, cx));
        }
        self.conflict_panel.apply_pending(window, cx);
        #[cfg(debug_assertions)]
        if let Some(kind) = self.debug_remote.take() {
            cx.defer_in(window, move |this, window, cx| {
                this.run_remote(kind, window, cx)
            });
        }
        let loaded = matches!(self.state, RepoState::Loaded(_));
        if matches!(self.state, RepoState::Empty | RepoState::Failed(_)) {
            self.reveal_recent_focus(window, cx);
        }
        let body = match &self.state {
            RepoState::Empty => self.render_start(None, cx).into_any_element(),
            RepoState::Failed(error) => self.render_start(Some(error), cx).into_any_element(),
            RepoState::Loading(_) => v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .text_color(cx.palette().fg3)
                .child(t!("repo.loading").into_owned())
                .into_any_element(),
            RepoState::Loaded(repo) => self.render_repo(repo, cx).into_any_element(),
        };
        let status_bar = match &self.state {
            RepoState::Loaded(repo) => Some(self.render_status_bar(repo, cx).into_any_element()),
            _ => None,
        };
        let run = |kind: RemoteOpKind| {
            move |this: &mut Self, window: &mut Window, cx: &mut Context<Self>| {
                this.run_remote(kind, window, cx)
            }
        };
        let (fetch_, pull_, merge, rebase, ff, push_, force) = (
            run(RemoteOpKind::Fetch),
            run(RemoteOpKind::Pull(PullMode::Configured)),
            run(RemoteOpKind::Pull(PullMode::Merge)),
            run(RemoteOpKind::Pull(PullMode::Rebase)),
            run(RemoteOpKind::Pull(PullMode::FastForwardOnly)),
            run(RemoteOpKind::Push(ForceMode::None)),
            run(RemoteOpKind::Push(ForceMode::WithLease)),
        );
        v_flex()
            .size_full()
            .relative()
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(move |this, _: &Fetch, window, cx| fetch_(this, window, cx)))
            .on_action(cx.listener(move |this, _: &Pull, window, cx| pull_(this, window, cx)))
            .on_action(cx.listener(move |this, _: &PullMerge, window, cx| merge(this, window, cx)))
            .on_action(cx.listener(|this, _: &ApplyPatches, window, cx| {
                this.prompt_apply_patches(window, cx)
            }))
            .on_action(cx.listener(|this, _: &OpenInTerminal, window, cx| {
                this.open_repository_in(external::Program::Terminal, window, cx)
            }))
            .on_action(cx.listener(|this, _: &OpenInFileManager, window, cx| {
                this.open_repository_in(external::Program::FileManager, window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &CleanUntracked, window, cx| {
                    this.ask_clean(false, window, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &CleanIgnored, window, cx| this.ask_clean(true, window, cx)),
            )
            .on_action(cx.listener(|this, _: &ShowTrash, window, cx| this.open_trash(window, cx)))
            .on_action(
                cx.listener(|this, _: &AddSubmodule, window, cx| {
                    this.open_add_submodule(window, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &ShowLfsLocks, window, cx| this.show_lfs_locks(window, cx)),
            )
            .on_action(cx.listener(|this, _: &ShowDanglingCommits, _, cx| this.open_dangling(cx)))
            .on_action(
                cx.listener(move |this, _: &PullRebase, window, cx| rebase(this, window, cx)),
            )
            .on_action(
                cx.listener(move |this, _: &PullFastForward, window, cx| ff(this, window, cx)),
            )
            .on_action(cx.listener(move |this, _: &Push, window, cx| push_(this, window, cx)))
            .on_action(cx.listener(move |this, _: &ForcePush, window, cx| force(this, window, cx)))
            .on_action(cx.listener(|this, _: &CancelRemote, _, cx| this.cancel_remote(cx)))
            .on_action(cx.listener(|this, _: &DownloadLfsFiles, window, cx| {
                this.run_remote(RemoteOpKind::LfsPull, window, cx)
            }))
            .on_action(cx.listener(|this, _: &FetchLfsObjects, window, cx| {
                this.run_remote(RemoteOpKind::LfsFetch, window, cx)
            }))
            .on_action(cx.listener(|this, _: &FetchAllLfsObjects, window, cx| {
                this.run_remote(RemoteOpKind::LfsFetchAll, window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &PruneLfsObjects, window, cx| this.ask_lfs_prune(window, cx)),
            )
            .on_action(cx.listener(|this, _: &DeepenHistory, window, cx| {
                let how = Deepen::By(sync::DEEPEN_BY);
                this.run_remote(RemoteOpKind::Deepen(how), window, cx)
            }))
            .on_action(cx.listener(|this, _: &FetchAllHistory, window, cx| {
                this.run_remote(RemoteOpKind::Deepen(Deepen::All), window, cx)
            }))
            .on_action(cx.listener(|this, _: &ToggleBackgroundFetch, window, cx| {
                this.toggle_background_fetch(window, cx)
            }))
            .on_action(cx.listener(|this, _: &FetchEvery5, window, cx| {
                this.set_fetch_interval(5, window, cx)
            }))
            .on_action(cx.listener(|this, _: &FetchEvery10, window, cx| {
                this.set_fetch_interval(10, window, cx)
            }))
            .on_action(cx.listener(|this, _: &FetchEvery15, window, cx| {
                this.set_fetch_interval(15, window, cx)
            }))
            .on_action(cx.listener(|this, _: &FetchEvery30, window, cx| {
                this.set_fetch_interval(30, window, cx)
            }))
            .on_action(cx.listener(|this, _: &FetchEvery60, window, cx| {
                this.set_fetch_interval(60, window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &UndoOperation, window, cx| {
                    this.undo_last(false, window, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &RedoOperation, window, cx| this.undo_last(true, window, cx)),
            )
            .on_action(cx.listener(|this, _: &Commit, _, cx| this.commit_staged(cx)))
            .on_action(cx.listener(|this, _: &StageAll, _, cx| this.stage_all(cx)))
            .on_action(cx.listener(|this, _: &UnstageAll, _, cx| this.unstage_all(cx)))
            .on_action(cx.listener(|this, _: &StashChanges, window, cx| this.ask_stash(window, cx)))
            .on_action(cx.listener(|this, _: &PopStash, window, cx| this.pop_stash(window, cx)))
            .on_action(cx.listener(|this, _: &NewBranch, window, cx| {
                if !this.is_start_screen() {
                    this.open_new_branch(None, window, cx)
                }
            }))
            .on_action(cx.listener(|this, _: &NewTag, window, cx| this.new_tag_at_head(window, cx)))
            .on_action(cx.listener(|this, _: &AddRemote, window, cx| {
                if !this.is_start_screen() {
                    this.open_add_remote(window, cx)
                }
            }))
            .on_action(cx.listener(|this, _: &AddWorktree, window, cx| {
                if !this.is_start_screen() {
                    this.open_add_worktree(None, window, cx)
                }
            }))
            .on_action(cx.listener(|this, _: &FocusNextPanel, window, cx| {
                this.cycle_focus(true, window, cx)
            }))
            .on_action(cx.listener(|this, _: &FocusPreviousPanel, window, cx| {
                this.cycle_focus(false, window, cx)
            }))
            .bg(cx.palette().bg)
            .text_color(cx.palette().fg)
            .font_family(gitweft_ui::current::ui_font())
            .when(loaded, |this| this.child(self.render_toolbar(cx)))
            // While the conflict tool is open, its header shows the operation instead.
            .when_some(
                self.operation.filter(|op| {
                    loaded && (self.conflict_view.is_none() || matches!(op, Operation::Bisect))
                }),
                |this, operation| {
                    if matches!(operation, Operation::Bisect) {
                        this.child(self.render_bisect_banner(cx).into_any_element())
                    } else {
                        this.child(
                            self.render_operation_banner(operation, cx)
                                .into_any_element(),
                        )
                    }
                },
            )
            .when(
                self.commit_graph_banner != CommitGraphBanner::Hidden,
                |this| this.child(self.render_commit_graph_banner(cx)),
            )
            .when(self.watch_limited, |this| {
                this.child(self.render_watch_banner(cx))
            })
            .when(loaded, |this| {
                this.children(self.render_untrusted_banner(cx))
            })
            .when(loaded, |this| this.children(self.render_sync_banners(cx)))
            .child(div().flex_1().min_h_0().child(body))
            .children(status_bar)
    }
}

/// Closing a tab stops what it started: git sees the same cancel as from Cancel.
impl Drop for Workspace {
    fn drop(&mut self) {
        if let Some(op) = &self.remote_op {
            op.cancel.cancel();
        }
        if let Some(op) = &self.clone_op {
            op.cancel.cancel();
        }
        if let Some(cancel) = &self.sync.fetch_running {
            cancel.cancel();
        }
        // Long jobs of a tab that's gone stop too: a bisect run would otherwise go on
        // checking out commits with nothing left to show it.
        if let Some(running) = &self.bisect_running {
            running.cancel.cancel();
        }
        for cancel in [&self.dangling_cancel, &self.pickaxe_cancel]
            .into_iter()
            .flatten()
        {
            cancel.cancel();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{choice_width, in_worktree, other_worktree_branches, stash_label, worktree_in};
    use gitweft_git::stash::StashEntry;
    use gitweft_git::worktrees::WorktreeInfo;
    use gpui_kit::px;
    use std::path::{Path, PathBuf};

    #[test]
    fn a_chosen_folder_with_files_gets_the_worktree_inside_it() {
        let root = std::env::temp_dir().join(format!("gitweft-worktree-in-{}", std::process::id()));
        let empty = root.join("empty");
        let full = root.join("full");
        std::fs::create_dir_all(&empty).unwrap();
        std::fs::create_dir_all(&full).unwrap();
        std::fs::write(full.join("notes.txt"), "x").unwrap();
        let default = Path::new("/somewhere/repo-feature");
        assert_eq!(worktree_in(&empty, default), empty);
        assert_eq!(worktree_in(&full, default), full.join("repo-feature"));
        // A folder that doesn't exist yet: git creates it.
        let new = root.join("new");
        assert_eq!(worktree_in(&new, default), new);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_graph_marks_branches_of_the_other_worktrees_only() {
        let info = |path: &str, branch: Option<&str>| WorktreeInfo {
            path: PathBuf::from(path),
            head: None,
            branch: branch.map(gitweft_git::gix::bstr::BString::from),
            bare: false,
            is_main: false,
            locked: None,
            prunable: None,
        };
        let worktrees = [
            info("/r", Some("refs/heads/main")),
            info("/r-wt", Some("refs/heads/feature/x")),
            info("/r-detached", None),
        ];
        let marked = other_worktree_branches(&worktrees, Some(Path::new("/r")));
        assert_eq!(marked.len(), 1);
        assert_eq!(marked["feature/x"].as_ref(), "r-wt");
        // From the linked worktree, main is the other one's.
        let marked = other_worktree_branches(&worktrees, Some(Path::new("/r-wt")));
        assert_eq!(marked.keys().collect::<Vec<_>>(), ["main"]);
        // A bare repository's tab has no worktree of its own.
        assert_eq!(other_worktree_branches(&worktrees, None).len(), 2);
    }

    #[test]
    fn stashes_are_named_by_their_message_or_where_they_were_made() {
        let label = |message: &str| {
            stash_label(&StashEntry {
                index: 0,
                id: gitweft_git::ObjectId::null(gitweft_git::gix::hash::Kind::Sha1),
                message: message.to_owned(),
                time: 0,
            })
        };
        assert_eq!(label("On main: half-done login"), "half-done login");
        assert_eq!(label("On main: a: b"), "a: b");
        assert_eq!(
            label("WIP on main: 1a2b3c4 Add the parser"),
            "Changes on main, on top of “Add the parser”"
        );
        assert_eq!(
            label("WIP on (no branch): 1a2b3c4 x"),
            "Changes on (no branch), on top of “x”"
        );
        assert_eq!(label("odd"), "odd");
    }

    #[test]
    fn a_subfolder_is_in_the_worktree_unless_another_repository_owns_it() {
        let root = std::env::temp_dir().join(format!("gitweft-in-worktree-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for dir in [".git", "src/deep", "sub/.git", "sub/inner", "wt"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        std::fs::write(root.join("wt/.git"), "gitdir: elsewhere\n").unwrap();
        assert!(in_worktree(&root, &root.join("src")));
        assert!(in_worktree(&root, &root.join("src/deep/../deep")));
        assert!(!in_worktree(&root, &root.join("sub")), "a submodule");
        assert!(!in_worktree(&root, &root.join("sub/inner")));
        assert!(!in_worktree(&root, &root.join("wt")), "a linked worktree");
        assert!(!in_worktree(&root.join("src"), &root));
        assert!(!in_worktree(&root, &std::env::temp_dir()));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_choice_dialog_grows_with_its_buttons() {
        assert_eq!(choice_width(["Cancel", "Merge"].into_iter(), 1.0), px(480.));
        let four = [
            "Cancel",
            "Rebase, keeping merges",
            "Rebase onto feature/search",
            "Merge into feature/search",
        ];
        let width = choice_width(four.into_iter(), 1.0);
        assert!(width > px(620.), "{width:?}");
        assert_eq!(choice_width(four.into_iter(), 1.5), width * 1.5);
    }

    /// Every view the workspace can show instead of the graph is in `centre_views!`, so
    /// closing, covering and focus know about it. The graph and the conflict tool aren't.
    #[test]
    fn every_centre_view_is_in_the_centre_list() {
        let source = include_str!("workspace.rs");
        let listed = source
            .split("macro_rules! centre_views {")
            .nth(1)
            .and_then(|rest| rest.split("\n}\n").next())
            .unwrap();
        let fields: Vec<&str> = source
            .lines()
            .filter_map(|line| {
                let (name, ty) = line.trim().split_once(": Option<Entity<")?;
                ty.ends_with("View>>,").then_some(name)
            })
            .filter(|name| !["graph_view", "conflict_view"].contains(name))
            .collect();
        assert!(fields.len() >= 10, "{fields:?}");
        for field in fields {
            assert!(
                listed.contains(&format!(" {field} close_")),
                "{field} is missing from centre_views!"
            );
        }
    }
}
