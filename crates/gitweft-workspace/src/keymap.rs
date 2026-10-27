//! Key bindings the user can change: the commands the Keyboard page lists, their default
//! keys as `bind_keys` registered them, and the user's keymap file (`keymap.json`) on top.
//!
//! GPUI can't take a single binding out of its keymap, and clearing it drops the kit's
//! bindings too. So the whole keymap as it stands after start-up (the kit's and ours) is
//! kept as the base, and applying the user's keymap rebuilds it: the base without the
//! defaults of the commands the user changed, then the user's keys for those.

use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gitweft_core::keymap::{KeymapFile, Overrides, keymap_path};
use gpui_kit::*;
use rust_i18n::t;

use crate::{
    blame_view, code_keys, compare_view, conflict_view, diff_view, file_view, find, graph_view,
    history_view, list_keys, rebase_view, reflog_view, settings_window, shell, tree_view,
    workspace,
};

/// The Keyboard page's groups (locale keys), in its order.
pub(crate) const GROUPS: [&str; 5] = [
    "keyboard.group.window",
    "keyboard.group.repository",
    "keyboard.group.graph",
    "keyboard.group.lists",
    "keyboard.group.views",
];

/// Keys that are only modifiers: pressed alone while recording, they wait for the rest.
const MODIFIER_KEYS: [&str; 7] = [
    "shift", "control", "alt", "platform", "function", "super", "ctrl",
];
/// A recording ends this long after its last chord (GPUI waits about as long between the
/// chords of a sequence), or at once with [`MAX_CHORDS`] chords.
const PAUSE: Duration = Duration::from_millis(1000);
const MAX_CHORDS: usize = 4;

/// A command whose keys the user can change.
pub(crate) struct Bindable {
    pub action: Box<dyn Action>,
    pub label: String,
    /// The key contexts its bindings live in (none: every window).
    pub contexts: Vec<&'static str>,
    /// One of [`GROUPS`].
    pub group: &'static str,
}

impl Bindable {
    /// Its name in `keymap.json` (GPUI's, e.g. `workspace::Fetch`).
    pub(crate) fn name(&self) -> &'static str {
        self.action.name()
    }
}

fn command(
    group: &'static str,
    context: Option<&'static str>,
    action: impl Action,
    label: impl Into<String>,
) -> Bindable {
    command_in(group, context.as_slice(), action, label)
}

/// A command bound in several key contexts (the list keys).
fn command_in(
    group: &'static str,
    contexts: &[&'static str],
    action: impl Action,
    label: impl Into<String>,
) -> Bindable {
    Bindable {
        action: Box::new(action),
        label: label.into(),
        contexts: contexts.to_vec(),
        group,
    }
}

/// Every command the Keyboard page lists, in its order.
pub(crate) fn bindable() -> Vec<Bindable> {
    let [window, repository, graph, lists, views] = GROUPS;
    let shell = Some(shell::KEY_CONTEXT);
    let tab = |n: usize| t!("keyboard.action.activate_tab", n = n).into_owned();
    let workspace = Some(workspace::KEY_CONTEXT);
    let graph_context = Some(graph_view::CONTEXT);
    vec![
        command(
            window,
            shell,
            shell::OpenCommandPalette,
            t!("keyboard.action.palette"),
        ),
        command(
            window,
            None,
            settings_window::OpenSettings,
            t!("keyboard.action.settings"),
        ),
        command(window, shell, shell::NewTab, t!("palette.tab_new")),
        command(window, shell, shell::CloseTab, t!("palette.tab_close")),
        command(window, shell, shell::NextTab, t!("palette.tab_next")),
        command(
            window,
            shell,
            shell::PreviousTab,
            t!("palette.tab_previous"),
        ),
        command(window, shell, shell::ActivateTab1, tab(1)),
        command(window, shell, shell::ActivateTab2, tab(2)),
        command(window, shell, shell::ActivateTab3, tab(3)),
        command(window, shell, shell::ActivateTab4, tab(4)),
        command(window, shell, shell::ActivateTab5, tab(5)),
        command(window, shell, shell::ActivateTab6, tab(6)),
        command(window, shell, shell::ActivateTab7, tab(7)),
        command(window, shell, shell::ActivateTab8, tab(8)),
        command(
            window,
            shell,
            shell::ActivateLastTab,
            t!("keyboard.action.last_tab"),
        ),
        command(
            repository,
            workspace,
            workspace::UndoOperation,
            t!("toolbar.undo"),
        ),
        command(
            repository,
            workspace,
            workspace::RedoOperation,
            t!("toolbar.redo"),
        ),
        command(repository, workspace, workspace::Fetch, t!("toolbar.fetch")),
        command(repository, workspace, workspace::Pull, t!("toolbar.pull")),
        command(
            repository,
            workspace,
            workspace::PullMerge,
            t!("toolbar.pull_merge"),
        ),
        command(
            repository,
            workspace,
            workspace::PullRebase,
            t!("toolbar.pull_rebase"),
        ),
        command(
            repository,
            workspace,
            workspace::PullFastForward,
            t!("toolbar.pull_ff_only"),
        ),
        command(repository, workspace, workspace::Push, t!("toolbar.push")),
        command(
            repository,
            workspace,
            workspace::ForcePush,
            t!("toolbar.force_push"),
        ),
        command(
            repository,
            workspace,
            workspace::CancelRemote,
            t!("palette.cancel_remote"),
        ),
        // In text fields too: Ctrl+Enter in the commit message.
        command_in(
            repository,
            &[workspace::KEY_CONTEXT, workspace::INPUT_KEY_CONTEXT],
            workspace::Commit,
            t!("palette.commit"),
        ),
        command(
            repository,
            workspace,
            workspace::StageAll,
            t!("staging.stage_all"),
        ),
        command(
            repository,
            workspace,
            workspace::UnstageAll,
            t!("staging.unstage_all"),
        ),
        command(
            repository,
            workspace,
            workspace::StashChanges,
            t!("palette.stash"),
        ),
        command(
            repository,
            workspace,
            workspace::PopStash,
            t!("palette.pop"),
        ),
        command(
            repository,
            workspace,
            workspace::NewBranch,
            t!("branch.new"),
        ),
        command(
            repository,
            workspace,
            workspace::NewTag,
            t!("palette.new_tag"),
        ),
        command(
            repository,
            workspace,
            workspace::AddRemote,
            t!("remote.add_item"),
        ),
        command(
            repository,
            workspace,
            workspace::AddWorktree,
            t!("worktree.add_item"),
        ),
        command(
            repository,
            workspace,
            workspace::OpenInTerminal,
            t!("external.open_terminal"),
        ),
        command(
            repository,
            workspace,
            workspace::OpenInFileManager,
            t!("external.open_file_manager"),
        ),
        command(
            repository,
            workspace,
            workspace::ApplyPatches,
            t!("files.apply_patches"),
        ),
        command(
            repository,
            workspace,
            workspace::CleanUntracked,
            t!("files.clean_untracked"),
        ),
        command(
            repository,
            workspace,
            workspace::CleanIgnored,
            t!("files.clean_ignored"),
        ),
        command(
            repository,
            workspace,
            workspace::ShowTrash,
            t!("trash.show"),
        ),
        command(
            repository,
            workspace,
            workspace::AddSubmodule,
            t!("submodule.add_menu"),
        ),
        command(
            repository,
            workspace,
            workspace::ShowLfsLocks,
            t!("lfs.locks_menu"),
        ),
        command(
            repository,
            workspace,
            workspace::ShowDanglingCommits,
            t!("dangling.show"),
        ),
        command(
            repository,
            workspace,
            workspace::DownloadLfsFiles,
            t!("lfs.pull"),
        ),
        command(
            repository,
            workspace,
            workspace::FetchLfsObjects,
            t!("lfs.fetch"),
        ),
        command(
            repository,
            workspace,
            workspace::FetchAllLfsObjects,
            t!("lfs.fetch_all"),
        ),
        command(
            repository,
            workspace,
            workspace::PruneLfsObjects,
            t!("lfs.prune"),
        ),
        command(
            repository,
            workspace,
            workspace::DeepenHistory,
            t!("keyboard.action.deepen"),
        ),
        command(
            repository,
            workspace,
            workspace::FetchAllHistory,
            t!("shallow.unshallow"),
        ),
        command(
            repository,
            workspace,
            workspace::ToggleBackgroundFetch,
            t!("keyboard.action.background_fetch"),
        ),
        command(
            graph,
            graph_context,
            graph_view::SelectNext,
            t!("keyboard.action.next"),
        ),
        command(
            graph,
            graph_context,
            graph_view::SelectPrevious,
            t!("keyboard.action.previous"),
        ),
        command(
            graph,
            graph_context,
            graph_view::SelectFirst,
            t!("keyboard.action.first"),
        ),
        command(
            graph,
            graph_context,
            graph_view::SelectLast,
            t!("keyboard.action.last"),
        ),
        command(
            graph,
            graph_context,
            graph_view::ExtendNext,
            t!("keyboard.action.extend_next_commit"),
        ),
        command(
            graph,
            graph_context,
            graph_view::ExtendPrevious,
            t!("keyboard.action.extend_previous_commit"),
        ),
        command(
            graph,
            graph_context,
            graph_view::ExtendFirst,
            t!("keyboard.action.extend_first_commit"),
        ),
        command(
            graph,
            graph_context,
            graph_view::ExtendLast,
            t!("keyboard.action.extend_last_commit"),
        ),
        command(
            graph,
            graph_context,
            graph_view::PageDown,
            t!("keyboard.action.page_down"),
        ),
        command(
            graph,
            graph_context,
            graph_view::PageUp,
            t!("keyboard.action.page_up"),
        ),
        command(
            graph,
            graph_context,
            graph_view::FocusSearch,
            t!("palette.search"),
        ),
        command(
            views,
            Some(diff_view::CONTEXT),
            diff_view::CloseDiff,
            t!("keyboard.action.close_diff"),
        ),
        command(
            views,
            Some(blame_view::CONTEXT),
            blame_view::CloseBlame,
            t!("keyboard.action.close_blame"),
        ),
        command(
            views,
            Some(file_view::CONTEXT),
            file_view::CloseFile,
            t!("keyboard.action.close_file"),
        ),
        // Find in the code views.
        command_in(
            views,
            &[
                diff_view::CONTEXT,
                file_view::CONTEXT,
                blame_view::CONTEXT,
                conflict_view::CONTEXT,
            ],
            find::OpenFind,
            t!("keyboard.action.find_open"),
        ),
        command(
            views,
            Some(find::CONTEXT),
            find::FindNext,
            t!("keyboard.action.find_next"),
        ),
        command(
            views,
            Some(find::CONTEXT),
            find::FindPrevious,
            t!("keyboard.action.find_previous"),
        ),
        command(
            views,
            Some(find::CONTEXT),
            find::ToggleFindCase,
            t!("keyboard.action.find_case"),
        ),
        command(
            views,
            Some(find::CONTEXT),
            find::CloseFind,
            t!("keyboard.action.find_close"),
        ),
        command(
            views,
            Some(history_view::CONTEXT),
            history_view::CloseHistory,
            t!("keyboard.action.close_history"),
        ),
        command(
            views,
            Some(tree_view::CONTEXT),
            tree_view::CloseTree,
            t!("keyboard.action.close_tree"),
        ),
        command(
            views,
            Some(reflog_view::CONTEXT),
            reflog_view::CloseReflog,
            t!("keyboard.action.close_reflog"),
        ),
        command(
            views,
            Some(crate::dangling_view::CONTEXT),
            crate::dangling_view::CloseDangling,
            t!("keyboard.action.close_dangling"),
        ),
        command(
            views,
            Some(crate::trash_view::CONTEXT),
            crate::trash_view::CloseTrash,
            t!("keyboard.action.close_trash"),
        ),
        command(
            views,
            Some(compare_view::CONTEXT),
            compare_view::CloseCompare,
            t!("keyboard.action.close_compare"),
        ),
        command(
            views,
            Some(conflict_view::CONTEXT),
            conflict_view::CloseConflict,
            t!("keyboard.action.close_conflict"),
        ),
        command(
            views,
            Some(conflict_view::PANES),
            conflict_view::OtherPane,
            t!("keyboard.action.other_pane"),
        ),
        command(
            views,
            Some(rebase_view::CONTEXT),
            rebase_view::MoveUp,
            t!("keyboard.action.move_up"),
        ),
        command(
            views,
            Some(rebase_view::CONTEXT),
            rebase_view::MoveDown,
            t!("keyboard.action.move_down"),
        ),
        command(
            views,
            Some(rebase_view::CONTEXT),
            rebase_view::PickRow,
            t!("keyboard.action.rebase_pick"),
        ),
        command(
            views,
            Some(rebase_view::CONTEXT),
            rebase_view::RewordRow,
            t!("keyboard.action.rebase_reword"),
        ),
        command(
            views,
            Some(rebase_view::CONTEXT),
            rebase_view::EditRow,
            t!("keyboard.action.rebase_edit"),
        ),
        command(
            views,
            Some(rebase_view::CONTEXT),
            rebase_view::SquashRow,
            t!("keyboard.action.rebase_squash"),
        ),
        command(
            views,
            Some(rebase_view::CONTEXT),
            rebase_view::FixupRow,
            t!("keyboard.action.rebase_fixup"),
        ),
        command(
            views,
            Some(rebase_view::CONTEXT),
            rebase_view::DropRow,
            t!("keyboard.action.rebase_drop"),
        ),
        command(
            views,
            Some(rebase_view::CONTEXT),
            rebase_view::CloseRebase,
            t!("keyboard.action.rebase_close"),
        ),
        command(
            window,
            workspace,
            workspace::FocusNextPanel,
            t!("keyboard.action.next_panel"),
        ),
        command(
            window,
            workspace,
            workspace::FocusPreviousPanel,
            t!("keyboard.action.previous_panel"),
        ),
        command_in(
            lists,
            &list_keys::LISTS,
            list_keys::NextRow,
            t!("keyboard.action.next_row"),
        ),
        command_in(
            lists,
            &list_keys::LISTS,
            list_keys::PreviousRow,
            t!("keyboard.action.previous_row"),
        ),
        command_in(
            lists,
            &list_keys::LISTS,
            list_keys::FirstRow,
            t!("keyboard.action.first_row"),
        ),
        command_in(
            lists,
            &list_keys::LISTS,
            list_keys::LastRow,
            t!("keyboard.action.last_row"),
        ),
        command_in(
            lists,
            &list_keys::LISTS,
            list_keys::OpenRow,
            t!("keyboard.action.open_row"),
        ),
        command_in(
            lists,
            &list_keys::TOGGLE_LISTS,
            list_keys::ToggleRow,
            t!("keyboard.action.toggle_row"),
        ),
        command_in(
            lists,
            &list_keys::DELETE_LISTS,
            list_keys::DeleteRow,
            t!("keyboard.action.delete_row"),
        ),
        command_in(
            lists,
            &list_keys::MENU_LISTS,
            list_keys::RowMenu,
            t!("keyboard.action.row_menu"),
        ),
        command_in(
            views,
            &code_keys::CODE_VIEWS,
            code_keys::NextPage,
            t!("keyboard.action.page_down"),
        ),
        command_in(
            views,
            &code_keys::CODE_VIEWS,
            code_keys::PreviousPage,
            t!("keyboard.action.page_up"),
        ),
        command_in(
            views,
            &code_keys::CODE_VIEWS,
            code_keys::ScrollLeft,
            t!("keyboard.action.scroll_left"),
        ),
        command_in(
            views,
            &code_keys::CODE_VIEWS,
            code_keys::ScrollRight,
            t!("keyboard.action.scroll_right"),
        ),
        command_in(
            views,
            &code_keys::HUNK_VIEWS,
            code_keys::NextHunk,
            t!("keyboard.action.next_hunk"),
        ),
        command_in(
            views,
            &code_keys::HUNK_VIEWS,
            code_keys::PreviousHunk,
            t!("keyboard.action.previous_hunk"),
        ),
        command_in(
            lists,
            &list_keys::EXTEND_LISTS,
            list_keys::ExtendToNextRow,
            t!("keyboard.action.extend_next"),
        ),
        command_in(
            lists,
            &list_keys::EXTEND_LISTS,
            list_keys::ExtendToPreviousRow,
            t!("keyboard.action.extend_previous"),
        ),
        command_in(
            lists,
            &list_keys::EXTEND_LISTS,
            list_keys::ExtendToFirstRow,
            t!("keyboard.action.extend_first"),
        ),
        command_in(
            lists,
            &list_keys::EXTEND_LISTS,
            list_keys::ExtendToLastRow,
            t!("keyboard.action.extend_last"),
        ),
        command_in(
            lists,
            &[list_keys::STAGING],
            list_keys::SelectAllRows,
            t!("keyboard.action.select_all_rows"),
        ),
        command_in(
            lists,
            &[tree_view::CONTEXT],
            tree_view::CollapseOrParent,
            t!("keyboard.action.tree_collapse"),
        ),
        command_in(
            lists,
            &[tree_view::CONTEXT],
            tree_view::ExpandOrChild,
            t!("keyboard.action.tree_expand"),
        ),
    ]
}

/// The keymap in force: the base it is rebuilt from, the user's changes and their file.
pub(crate) struct KeymapStore {
    base: Vec<KeyBinding>,
    overrides: Overrides,
    file: Arc<Mutex<KeymapFile>>,
    /// The command whose new keys the Keyboard page is waiting for.
    recording: Option<&'static str>,
    /// The chords pressed so far in that recording ("ctrl-k", "ctrl-s").
    pending: Vec<String>,
    /// Ends the recording once the chords pause; the next chord replaces it.
    _pause: Option<Task<()>>,
    _recorder: Subscription,
}

impl Global for KeymapStore {}

impl KeymapStore {
    /// Called once every default binding is registered: keeps them as the base, reads the
    /// user's keymap and applies it.
    pub(crate) fn init(cx: &mut App) {
        Self::init_with_file(cx, keymap_path());
    }

    /// [`Self::init`] with the user's keymap in `path` (tests).
    pub(crate) fn init_with_file(cx: &mut App, path: std::path::PathBuf) {
        let base = cx.key_bindings().borrow().bindings().cloned().collect();
        let (file, overrides) = KeymapFile::open(path);
        let recorder = cx.intercept_keystrokes(record);
        cx.set_global(Self {
            base,
            overrides,
            file: Arc::new(Mutex::new(file)),
            recording: None,
            pending: Vec::new(),
            _pause: None,
            _recorder: recorder,
        });
        apply(cx);
    }

    /// The keys `name` has by default, each a keystroke sequence ("ctrl-t", "g g"), once
    /// each (a list key is bound in several contexts).
    pub(crate) fn default_keys(&self, name: &str) -> Vec<String> {
        let mut keys: Vec<String> = Vec::new();
        for binding in self.base.iter().filter(|b| b.action().name() == name) {
            let strokes: Vec<String> = binding.keystrokes().iter().map(|k| k.unparse()).collect();
            let key = strokes.join(" ");
            if !keys.contains(&key) {
                keys.push(key);
            }
        }
        keys
    }

    /// The keys `name` has now: the user's, else its defaults.
    pub(crate) fn keys(&self, name: &str) -> Vec<String> {
        self.overrides
            .get(name)
            .cloned()
            .unwrap_or_else(|| self.default_keys(name))
    }

    /// Whether the user changed `name`'s keys.
    pub(crate) fn is_changed(&self, name: &str) -> bool {
        self.overrides.contains_key(name)
    }

    pub(crate) fn recording(&self) -> Option<&'static str> {
        self.recording
    }

    /// The chords pressed so far while recording.
    pub(crate) fn pending(&self) -> &[String] {
        &self.pending
    }

    /// The other commands whose keys clash with one of `command`'s ([`keys_clash`]) in a
    /// key context that can be active at the same time (the same one, or a binding without
    /// a context).
    pub(crate) fn conflicts<'a>(&self, command: &Bindable, all: &'a [Bindable]) -> Vec<&'a str> {
        let keys = self.keys(command.name());
        all.iter()
            .filter(|other| other.name() != command.name())
            .filter(|other| {
                other.contexts.is_empty()
                    || command.contexts.is_empty()
                    || other.contexts.iter().any(|c| command.contexts.contains(c))
            })
            .filter(|other| {
                self.keys(other.name())
                    .iter()
                    .any(|key| keys.iter().any(|mine| keys_clash(mine, key)))
            })
            .map(|other| other.label.as_str())
            .collect()
    }

    /// Waits for the next keys pressed to become `name`'s (`None`: stops waiting).
    pub(crate) fn record(cx: &mut App, name: Option<&'static str>) {
        cx.update_global(|store: &mut Self, _| {
            store.recording = name;
            store.pending.clear();
            store._pause = None;
        });
    }

    /// Ends `name`'s recording once its chords paused: they become its key, as one
    /// sequence.
    fn finish(cx: &mut App, name: &'static str) {
        let store = cx.global_mut::<Self>();
        if store.recording != Some(name) || store.pending.is_empty() {
            return;
        }
        // This may run inside that task: let it finish rather than cancel it.
        if let Some(pause) = store._pause.take() {
            pause.detach();
        }
        let keys = store.pending.join(" ");
        Self::set(cx, name, Some(vec![keys]));
    }

    /// Gives `name` these keys (`None`: its defaults again; an empty list: none), applies the
    /// keymap and saves the file.
    pub(crate) fn set(cx: &mut App, name: &'static str, keys: Option<Vec<String>>) {
        cx.update_global(|store: &mut Self, _| {
            match keys {
                Some(keys) => store.overrides.insert(name.to_owned(), keys),
                None => store.overrides.remove(name),
            };
            store.recording = None;
            store.pending.clear();
            store._pause = None;
        });
        apply(cx);
        let store = cx.global::<Self>();
        let (file, overrides) = (store.file.clone(), store.overrides.clone());
        cx.background_spawn(async move {
            let saved = file.lock().map(|mut file| file.save(&overrides));
            if let Ok(Err(err)) = saved {
                eprintln!("gitweft: couldn't save the keymap: {err}");
            }
        })
        .detach();
    }
}

/// Rebuilds GPUI's keymap: the base without the defaults of the commands the user changed,
/// then the user's keys for them. Keys GPUI can't parse are skipped (and reported).
fn apply(cx: &mut App) {
    let commands = bindable();
    let store = cx.global::<KeymapStore>();
    let changed = |name: &str| {
        store.overrides.contains_key(name) && commands.iter().any(|command| command.name() == name)
    };
    let mut bindings: Vec<KeyBinding> = store
        .base
        .iter()
        .filter(|binding| !changed(binding.action().name()))
        .cloned()
        .collect();
    let mapper = cx.keyboard_mapper().clone();
    for command in &commands {
        let Some(keys) = store.overrides.get(command.name()) else {
            continue;
        };
        // Bound in each of its contexts (or once, everywhere).
        let contexts: Vec<Option<Rc<KeyBindingContextPredicate>>> = if command.contexts.is_empty() {
            vec![None]
        } else {
            command
                .contexts
                .iter()
                .filter_map(|context| KeyBindingContextPredicate::parse(context).ok())
                .map(|context| Some(Rc::new(context)))
                .collect()
        };
        for key in keys {
            for context in &contexts {
                match KeyBinding::load(
                    key,
                    command.action.boxed_clone(),
                    context.clone(),
                    false,
                    None,
                    mapper.as_ref(),
                ) {
                    Ok(binding) => bindings.push(binding),
                    Err(err) => {
                        eprintln!("gitweft: keymap.json: {}: {key:?}: {err}", command.name());
                        break;
                    }
                }
            }
        }
    }
    cx.clear_key_bindings();
    cx.bind_keys(bindings);
}

/// While the Keyboard page records, the keystrokes pressed (in any window) become the
/// command's key instead of doing what they're bound to: one chord, or a sequence of up
/// to [`MAX_CHORDS`] ("ctrl-k ctrl-s"), ended by a [`PAUSE`]. Escape alone cancels.
fn record(event: &KeystrokeEvent, window: &mut Window, cx: &mut App) {
    let Some(name) = cx
        .try_global::<KeymapStore>()
        .and_then(|store| store.recording)
    else {
        return;
    };
    // Only keys pressed in the settings window are recorded. One pressed in another window
    // (the settings window closed, or behind the main window) ends the recording and does
    // what it's bound to, instead of becoming a key without the user seeing it.
    if !crate::settings_window::is_settings_window(window, cx) {
        cx.defer(|cx| KeymapStore::record(cx, None));
        return;
    }
    let keystroke = &event.keystroke;
    if MODIFIER_KEYS.contains(&keystroke.key.as_str()) {
        return;
    }
    cx.stop_propagation();
    let cancel = keystroke.key == "escape" && !keystroke.modifiers.modified();
    let stroke = keystroke.unparse();
    // Not while GPUI is dispatching this very keystroke.
    cx.defer(move |cx| {
        if cancel {
            KeymapStore::record(cx, None);
            return;
        }
        let full =
            cx.update_global(|store: &mut KeymapStore, _| push_chord(&mut store.pending, stroke));
        if full {
            KeymapStore::finish(cx, name);
            return;
        }
        // Each chord starts the wait again: the new task replaces the last one.
        let pause = cx.spawn(async move |cx| {
            cx.background_executor().timer(PAUSE).await;
            cx.update(|cx| KeymapStore::finish(cx, name));
        });
        cx.update_global(|store: &mut KeymapStore, _| store._pause = Some(pause));
    });
}

/// Adds a chord to a recording; true once it holds as many as a key may have.
fn push_chord(pending: &mut Vec<String>, stroke: String) -> bool {
    pending.push(stroke);
    pending.len() >= MAX_CHORDS
}

/// Whether two keys (keystroke sequences, "ctrl-k ctrl-s") clash: the same, or one starts
/// the other (GPUI would then wait after the shorter one for the rest of the longer).
pub(crate) fn keys_clash(a: &str, b: &str) -> bool {
    let a: Vec<&str> = a.split_whitespace().collect();
    let b: Vec<&str> = b.split_whitespace().collect();
    let shared = a.len().min(b.len());
    shared > 0 && a[..shared] == b[..shared]
}

/// The keymap store in a GPUI test context: a changed key replaces the default in
/// GPUI's keymap and in the file, a reset brings it back, and in a window the new keys do
/// what the old ones did. The keymap file is the test's own (`init_with_file`).
#[cfg(test)]
mod store_tests {
    use std::cell::Cell;
    use std::path::{Path, PathBuf};

    use gitweft_core::keymap::KeymapFile;
    // Rust's own `#[test]`: `gpui_kit::*` (through `super::*`) brings GPUI's `test` macro.
    use std::prelude::v1::test;

    use super::*;
    use crate::settings_window::OpenSettings;

    /// "Open the settings": bound to Ctrl+, in every window by default.
    const NAME: &str = "settings::OpenSettings";

    /// The app's default binding, then the store over `dir/keymap.json`; returns the file.
    fn store(cx: &mut TestAppContext, dir: &Path) -> PathBuf {
        let path = dir.join("keymap.json");
        cx.update(|cx| {
            cx.bind_keys([KeyBinding::new("ctrl-,", OpenSettings, None)]);
            KeymapStore::init_with_file(cx, path.clone());
        });
        path
    }

    fn keys(cx: &TestAppContext) -> Vec<String> {
        cx.read(|cx| cx.global::<KeymapStore>().keys(NAME))
    }

    /// The keys GPUI's keymap binds the command to.
    fn bound(cx: &TestAppContext) -> Vec<String> {
        cx.update(|cx| {
            cx.key_bindings()
                .borrow()
                .bindings_for_action(&OpenSettings)
                .map(|binding| {
                    let strokes: Vec<String> =
                        binding.keystrokes().iter().map(|k| k.unparse()).collect();
                    strokes.join(" ")
                })
                .collect()
        })
    }

    fn saved(path: &Path) -> Option<Vec<String>> {
        KeymapFile::open(path.to_owned()).1.get(NAME).cloned()
    }

    #[test]
    fn a_changed_key_replaces_the_default_is_saved_and_can_be_reset() {
        let dir = tempfile::tempdir().unwrap();
        let mut cx = TestAppContext::single();
        let path = store(&mut cx, dir.path());
        assert_eq!(keys(&cx), ["ctrl-,"]);
        assert_eq!(bound(&cx), ["ctrl-,"]);

        cx.update(|cx| KeymapStore::set(cx, NAME, Some(vec!["ctrl-k ctrl-o".into()])));
        cx.run_until_parked();
        assert_eq!(keys(&cx), ["ctrl-k ctrl-o"]);
        assert!(cx.read(|cx| cx.global::<KeymapStore>().is_changed(NAME)));
        assert_eq!(bound(&cx), ["ctrl-k ctrl-o"], "the default is gone");
        assert_eq!(saved(&path), Some(vec!["ctrl-k ctrl-o".to_owned()]));

        cx.update(|cx| KeymapStore::set(cx, NAME, None));
        cx.run_until_parked();
        assert_eq!(bound(&cx), ["ctrl-,"]);
        assert_eq!(saved(&path), None);
    }

    /// A window's view that counts "Open the settings", focused from its first frame.
    struct Target {
        focus: FocusHandle,
        opened: Rc<Cell<u32>>,
    }

    impl Render for Target {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            self.focus.focus(window, cx);
            let opened = self.opened.clone();
            div()
                .size_full()
                .track_focus(&self.focus)
                .on_action(move |_: &OpenSettings, _, _| opened.set(opened.get() + 1))
        }
    }

    #[test]
    fn in_a_window_the_new_keys_do_what_the_old_key_did() {
        let dir = tempfile::tempdir().unwrap();
        let mut cx = TestAppContext::single();
        store(&mut cx, dir.path());
        cx.update(|cx| KeymapStore::set(cx, NAME, Some(vec!["ctrl-k ctrl-o".into()])));
        let opened = Rc::new(Cell::new(0));
        let count = opened.clone();
        let (_, window) = cx.add_window_view(|_, cx| Target {
            focus: cx.focus_handle(),
            opened: count,
        });

        window.simulate_keystrokes("ctrl-,");
        assert_eq!(opened.get(), 0, "the old key does nothing now");
        window.simulate_keystrokes("ctrl-k ctrl-o");
        assert_eq!(opened.get(), 1);
    }

    /// A window with a counting view under the kit's `Root` (as the settings window has),
    /// taken for the settings window when `settings`; returns it and its count.
    fn counting_window(
        cx: &mut TestAppContext,
        settings: bool,
    ) -> (gpui_kit::AnyWindowHandle, Rc<Cell<u32>>) {
        cx.update(gpui_kit::init);
        let opened = Rc::new(Cell::new(0));
        let count = opened.clone();
        let handle = cx.add_window(|window, cx| {
            let view = cx.new(|cx| Target {
                focus: cx.focus_handle(),
                opened: count,
            });
            gpui_kit::component::Root::new(view, window, cx)
        });
        if settings {
            cx.update(|cx| crate::settings_window::pretend_settings_window(handle, cx));
        }
        (handle.into(), opened)
    }

    fn recording(cx: &TestAppContext) -> Option<&'static str> {
        cx.read(|cx| cx.global::<KeymapStore>().recording())
    }

    #[test]
    fn keys_pressed_in_the_settings_window_become_the_commands_after_a_pause() {
        let dir = tempfile::tempdir().unwrap();
        let mut cx = TestAppContext::single();
        let path = store(&mut cx, dir.path());
        let (settings, opened) = counting_window(&mut cx, true);
        cx.update(|cx| KeymapStore::record(cx, Some(NAME)));

        cx.simulate_keystrokes(settings, "ctrl-, ctrl-o");
        assert_eq!(
            opened.get(),
            0,
            "a bound key pressed while recording is only recorded"
        );
        assert_eq!(keys(&cx), ["ctrl-,"], "not before the pause");
        assert_eq!(recording(&cx), Some(NAME));

        cx.executor().advance_clock(PAUSE);
        cx.run_until_parked();
        assert_eq!(keys(&cx), ["ctrl-, ctrl-o"]);
        assert_eq!(recording(&cx), None);
        assert_eq!(saved(&path), Some(vec!["ctrl-, ctrl-o".to_owned()]));
    }

    #[test]
    fn escape_cancels_a_recording_and_keys_work_again() {
        let dir = tempfile::tempdir().unwrap();
        let mut cx = TestAppContext::single();
        store(&mut cx, dir.path());
        let (settings, opened) = counting_window(&mut cx, true);
        cx.update(|cx| KeymapStore::record(cx, Some(NAME)));

        cx.simulate_keystrokes(settings, "escape");
        assert_eq!(recording(&cx), None);
        assert_eq!(keys(&cx), ["ctrl-,"]);
        cx.simulate_keystrokes(settings, "ctrl-,");
        assert_eq!(opened.get(), 1);
    }

    #[test]
    fn a_key_pressed_in_another_window_ends_the_recording_and_does_its_job() {
        let dir = tempfile::tempdir().unwrap();
        let mut cx = TestAppContext::single();
        store(&mut cx, dir.path());
        let (_settings, _) = counting_window(&mut cx, true);
        let (main, opened) = counting_window(&mut cx, false);
        cx.update(|cx| KeymapStore::record(cx, Some(NAME)));

        cx.simulate_keystrokes(main, "ctrl-,");
        assert_eq!(opened.get(), 1);
        assert_eq!(recording(&cx), None);
        assert_eq!(keys(&cx), ["ctrl-,"]);
    }

    #[test]
    fn the_last_chord_a_key_may_have_ends_the_recording_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let mut cx = TestAppContext::single();
        store(&mut cx, dir.path());
        let (settings, _) = counting_window(&mut cx, true);
        cx.update(|cx| KeymapStore::record(cx, Some(NAME)));

        cx.simulate_keystrokes(settings, "f1 f2 f3 f4");
        assert_eq!(recording(&cx), None, "no pause needed");
        assert_eq!(keys(&cx), ["f1 f2 f3 f4"]);
    }
}

#[cfg(test)]
mod tests {
    use super::{GROUPS, MAX_CHORDS, bindable, keys_clash, push_chord};
    use std::collections::HashSet;

    #[test]
    fn a_sequence_clashes_with_its_start_and_with_itself() {
        assert!(keys_clash("ctrl-t", "ctrl-t"));
        assert!(keys_clash("ctrl-k ctrl-s", "ctrl-k"));
        assert!(keys_clash("ctrl-k", "ctrl-k ctrl-s"));
        assert!(keys_clash("g g", "g  g"));
        assert!(!keys_clash("ctrl-k ctrl-s", "ctrl-k ctrl-t"));
        assert!(!keys_clash("ctrl-s", "ctrl-k ctrl-s"));
        assert!(!keys_clash("", "ctrl-k"));
    }

    #[test]
    fn a_recording_ends_at_the_most_chords_a_key_may_have() {
        let mut pending = Vec::new();
        for n in 1..MAX_CHORDS {
            assert!(!push_chord(&mut pending, format!("f{n}")));
        }
        assert!(push_chord(&mut pending, "enter".into()));
        assert_eq!(pending.len(), MAX_CHORDS);
        assert_eq!(pending.join(" "), "f1 f2 f3 enter");
    }

    #[test]
    fn every_command_is_listed_once_in_a_known_group() {
        let commands = bindable();
        let names: HashSet<&str> = commands.iter().map(|command| command.name()).collect();
        assert_eq!(names.len(), commands.len());
        assert!(
            commands
                .iter()
                .all(|command| GROUPS.contains(&command.group))
        );
        assert!(names.contains("workspace::Fetch"));
        assert!(names.contains("shell::OpenCommandPalette"));
    }
}
