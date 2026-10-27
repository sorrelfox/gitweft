//! Notifications and questions of tabs that aren't shown. A background tab's operation
//! that ends (a fetch, a pull) would otherwise pop its toast or dialog over another
//! repository; instead the tab keeps them, shows a dot, and they appear when it comes
//! forward.

use std::collections::{HashMap, HashSet};

use crate::toast::Toast;
use gpui_kit::*;

use crate::workspace::Workspace;

#[derive(Default)]
struct TabNotes {
    /// The tab shown (its workspace); `None` until the shell has drawn once.
    active: Option<EntityId>,
    /// What background tabs are keeping, in the order it came.
    pending: HashMap<EntityId, Vec<Toast>>,
    /// Background tabs with a question (a dialog) waiting to be asked.
    asking: HashSet<EntityId>,
}

impl Global for TabNotes {}

/// What a tab that isn't shown is keeping for when it comes forward.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Waiting {
    Nothing,
    /// Notifications only.
    Notes,
    /// A question, and perhaps notifications.
    Question,
}

/// Whether the workspace `id` is the tab shown (or nothing is known yet).
pub(crate) fn is_active(id: EntityId, cx: &App) -> bool {
    cx.try_global::<TabNotes>()
        .and_then(|notes| notes.active)
        .is_none_or(|active| active == id)
}

/// Shows `notification` when this workspace's tab is shown; otherwise keeps it for later.
pub(crate) fn note(notification: Toast, cx: &mut Context<Workspace>) {
    let id = cx.entity_id();
    if is_active(id, cx) {
        crate::toast::push(notification, cx);
    } else {
        cx.default_global::<TabNotes>()
            .pending
            .entry(id)
            .or_default()
            .push(notification);
        // The shell redraws the tab with its dot.
        cx.notify();
    }
}

/// Like [`note`], for work that ends without a window at hand (a save in the write lane):
/// the notification waits with its tab, and the shell hands it out when it next draws that
/// tab shown (at once for the tab shown: this redraws it).
pub(crate) fn note_soon(notification: Toast, cx: &mut Context<Workspace>) {
    let id = cx.entity_id();
    cx.default_global::<TabNotes>()
        .pending
        .entry(id)
        .or_default()
        .push(notification);
    cx.notify();
}

/// A failure the user should hear of, not only the log (#12): an error toast (through
/// [`note_soon`], so no window is needed), and the same line on stderr.
pub(crate) fn note_failure(text: String, cx: &mut Context<Workspace>) {
    eprintln!("gitweft: {text}");
    note_soon(Toast::error(text), cx);
}

/// This workspace's tab isn't shown and holds a question until it is (the workspace
/// keeps the question itself, as work for its window).
pub(crate) fn hold_question(cx: &mut Context<Workspace>) {
    let id = cx.entity_id();
    cx.default_global::<TabNotes>().asking.insert(id);
    cx.notify();
}

/// The shell shows the workspace `id` (called as it draws, every frame): what it kept in
/// the background, and what [`note_soon`] queued meanwhile, comes out now.
pub(crate) fn set_active(id: EntityId, cx: &mut App) {
    let notes = cx.default_global::<TabNotes>();
    if notes.active != Some(id) {
        notes.active = Some(id);
        // Its questions are asked as it draws.
        notes.asking.remove(&id);
    }
    let waiting = notes.pending.remove(&id).unwrap_or_default();
    for notification in waiting {
        crate::toast::push(notification, cx);
    }
}

/// What the workspace `id` keeps.
pub(crate) fn waiting(id: EntityId, cx: &App) -> Waiting {
    let Some(notes) = cx.try_global::<TabNotes>() else {
        return Waiting::Nothing;
    };
    if notes.asking.contains(&id) {
        Waiting::Question
    } else if notes.pending.get(&id).is_some_and(|kept| !kept.is_empty()) {
        Waiting::Notes
    } else {
        Waiting::Nothing
    }
}

/// A closed tab's notifications go with it.
pub(crate) fn forget(id: EntityId, cx: &mut App) {
    let notes = cx.default_global::<TabNotes>();
    notes.pending.remove(&id);
    notes.asking.remove(&id);
}
