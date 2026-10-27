//! Centre panel comparing a ref with the current branch: the commits each has that the
//! other lacks, and the files the ref changed since they diverged. A commit shows its
//! details; a file opens its diff on top of the comparison. For a review, it also shows
//! what changed since the review was last marked done: the new files, or the range-diff
//! after a rebase.

use std::ops::Range;

use gitweft_core::review::{
    Anchor, Comment, ExportInfo, ExportWords, NewComment, ReviewSession, ReviewedRange, Side,
    SinceReview, path_key, path_shown,
};
use gitweft_git::ObjectId;
use gitweft_git::gix::bstr::BString;
use gitweft_git::read::{ComparedCommit, Comparison, FileChange, RangeChange, RangePair};
use gitweft_ui::current::mono_font;
use gitweft_ui::truncated::truncated;
use gitweft_ui::widgets::{centre_header, list_row, path_label, placeholder, status_badge};
use gitweft_ui::{LookExt, Metrics, Palette};
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::{Disableable, Sizable, StyledExt, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

use crate::commit_columns::{self, CommitRow};
use crate::details_view::change_style;
use crate::list_keys::{
    self, FirstRow, LastRow, NextRow, OpenRow, PreviousRow, RowMenu, Step, ToggleRow,
};
use crate::palette::view_command;
use crate::staging_view::link;
use crate::util::format_time;

/// A range-diff pair's id cells ("3: 1a2b3c4d"), in design px.
const PAIR_ID_WIDTH: f32 = 96.;
/// A range-diff pair's word ("changed"), in design px.
const PAIR_LABEL_WIDTH: f32 = 72.;
/// A comment's place (`path:line`), in design px.
const PLACE_WIDTH: f32 = 200.;
pub(crate) const CONTEXT: &str = "CompareView";

actions!(compare, [CloseCompare]);

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([KeyBinding::new("escape", CloseCompare, Some(CONTEXT))]);
}

/// A comment to add: where, what it says, and who wrote it; [`CompareView::add_comment`]
/// adds the head it's written against and the time.
pub struct CommentDraft {
    pub path: String,
    pub side: Side,
    pub line: u32,
    pub anchor: Anchor,
    pub body: String,
    /// git's `user.name` (empty when it isn't set).
    pub author: String,
}

#[derive(Clone)]
pub enum CompareEvent {
    Close,
    /// Show the commit's details in the right panel.
    SelectCommit(ObjectId),
    /// Compare the other way round: the base becomes the compared side.
    Swap,
    /// Compare again, the same refs (the range-diff's matching changed).
    Recompare,
    /// Show the file's diff from `from` (the merge base, or the head reviewed last) to
    /// `to` (the compared ref). A diff of the whole review takes comments.
    OpenFile {
        from: ObjectId,
        to: ObjectId,
        change: FileChange,
        subtitle: String,
        /// Which review diff it is (comments go on different sides).
        kind: ReviewDiff,
    },
    /// Show how a rewritten commit's patch changed since the last review.
    OpenInterdiff {
        title: String,
        subtitle: String,
        lines: Vec<String>,
        /// The commit as it is now: a comment on the interdiff is about a line of its
        /// patch, so the file it names is read at this commit (#69).
        commit: ObjectId,
    },
    /// Row menus (#44): a commit selected in the graph, picked, or a branch made at it.
    ShowInGraph(ObjectId),
    CherryPick(ObjectId, String),
    CreateBranch(ObjectId),
    /// Row menus (#44): a file's blame or history at the compared head, or the whole
    /// comparison's change to it in the user's diff tool.
    Blame {
        path: BString,
        commit: ObjectId,
    },
    History {
        path: BString,
        commit: ObjectId,
    },
    DiffTool {
        from: ObjectId,
        to: ObjectId,
        change: FileChange,
    },
    /// A file was marked viewed (or not): the session to save.
    ReviewChanged(ReviewSession),
    /// Open a comment on its own (its text, with Edit, Resolve and Delete): one that has
    /// no place in the diffs shown now.
    OpenComment(u64),
    /// Hand the review, as Markdown, to `target`; `name` is the suggested file name,
    /// `head` the compared branch's head (where a note goes).
    ExportReview {
        markdown: String,
        name: String,
        head: ObjectId,
        target: ExportTarget,
    },
}

/// Where a review's Markdown goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportTarget {
    /// A file the user picks.
    File,
    Clipboard,
    /// A note on the compared head, in `refs/notes/review`.
    Note,
}

/// A diff opened from a review, and so where its comments can go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewDiff {
    /// From the merge base to the compared head: comments on either side.
    Whole,
    /// From the head reviewed last to the compared head: its old side is neither the
    /// review's base nor its head, so comments only go on the new side.
    Since,
}

impl EventEmitter<CompareEvent> for CompareView {}

impl Focusable for CompareView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    /// Commits only in the compared ref.
    Ahead,
    /// Commits only in the current branch.
    Behind,
    Files,
    /// The review's comments, outdated ones included.
    Comments,
    /// Comments deleted lately, each with Restore (#35).
    Deleted,
    /// What changed since the last review: new files, or the range-diff.
    Since,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    Header(Section),
    Commit(Section, usize),
    /// "… and N more" after a capped commit list.
    More(usize),
    File(usize),
    /// Comment `.0` of the review.
    Comment(usize),
    /// Deleted comment `.0` of the review.
    Deleted(usize),
    /// A file changed since the last review.
    SinceFile(usize),
    /// A commit pair of the range-diff since the last review.
    Pair(usize),
    /// Why there is nothing to show since the last review (an error).
    SinceNote,
}

enum Content {
    Loading,
    Loaded(Comparison),
    Failed(String),
}

pub struct CompareView {
    /// The current branch (or `HEAD` when detached).
    base_name: String,
    /// The ref compared with it.
    head_name: String,
    content: Content,
    rows: Vec<Row>,
    /// The row clicked last.
    selected: Option<Row>,
    scroll: UniformListScrollHandle,
    focus: FocusHandle,
    /// Take keyboard focus on the next frame, so Esc works.
    focus_on_render: bool,
    /// The review of the compared branch (none for a tag, or when detached): which
    /// files are viewed.
    review: Option<ReviewSession>,
    /// What changed since `.0`, the range reviewed last (when it was computed).
    since: Option<(ReviewedRange, Result<SinceReview, String>)>,
    /// A rewritten branch's range-diff pairs heavily reworked commits
    /// (`HARDER_CREATION_FACTOR`) instead of listing them as removed and added (#35).
    match_harder: bool,
    /// "… and N more" was chosen: the next comparison lists every commit (#44).
    show_all: bool,
}

impl CompareView {
    /// Whether the comparison has been read (the headless UI tests).
    #[cfg(test)]
    pub(crate) fn is_loaded(&self) -> bool {
        matches!(self.content, Content::Loaded(_))
    }

    pub fn new(base_name: String, head_name: String, cx: &mut Context<Self>) -> Self {
        Self {
            base_name,
            head_name,
            content: Content::Loading,
            rows: Vec::new(),
            selected: None,
            scroll: UniformListScrollHandle::new(),
            focus: cx.focus_handle(),
            focus_on_render: true,
            review: None,
            since: None,
            match_harder: false,
            show_all: false,
        }
    }

    /// Whether every commit is to be listed, not the newest 500 of each side.
    pub(crate) fn shows_all(&self) -> bool {
        self.show_all
    }

    /// Whether the range-diff since the last review should match reworked commits harder.
    pub fn match_harder(&self) -> bool {
        self.match_harder
    }

    /// Shows a (re)computed comparison with its review session, re-anchored by the job,
    /// and what changed since the last review. Once the view has a session, it only takes
    /// the comments' new places from it, so a mark made while the job ran isn't lost;
    /// moved comments are then saved.
    pub fn set_result(
        &mut self,
        result: Result<Comparison, String>,
        review: Option<ReviewSession>,
        since: Option<(ReviewedRange, Result<SinceReview, String>)>,
        cx: &mut Context<Self>,
    ) {
        self.since = since;
        if let (Some(mine), Some(fresh)) = (&mut self.review, &review) {
            if mine.take_positions(fresh) {
                cx.emit(CompareEvent::ReviewChanged(mine.clone()));
            }
        } else if self.review.is_none() {
            self.review = review;
        }
        // A file renamed since it was marked viewed keeps its mark (#35). Only the files the
        // compared ref has count: against the merge base a rename that changed the file
        // enough is a deletion plus an addition, and the deleted path must count as gone.
        if let (Ok(comparison), Some(review)) = (&result, &mut self.review) {
            let files: Vec<(String, ObjectId)> = comparison
                .files
                .iter()
                .zip(&comparison.blobs)
                .filter(|(change, _)| change.kind != gitweft_git::read::ChangeKind::Deleted)
                .map(|(change, blob)| (path_key(&change.path), *blob))
                .collect();
            if review.carry_marks(&files) {
                cx.emit(CompareEvent::ReviewChanged(review.clone()));
            }
        }
        self.content = match result {
            Ok(comparison) => Content::Loaded(comparison),
            Err(message) => Content::Failed(message),
        };
        self.rebuild_rows();
        // A reload can change what the rows are, so the old selection may point elsewhere.
        self.selected = None;
        cx.notify();
    }

    /// The range its file diffs show (merge base, compared ref), once loaded.
    pub fn range(&self) -> Option<(ObjectId, ObjectId)> {
        match &self.content {
            Content::Loaded(comparison) => Some((
                comparison.merge_base.unwrap_or(comparison.base),
                comparison.head,
            )),
            Content::Loading | Content::Failed(_) => None,
        }
    }

    /// The base's and the compared side's names, as shown.
    pub(crate) fn names(&self) -> (String, String) {
        (self.base_name.clone(), self.head_name.clone())
    }

    /// The review session, when this comparison is a review.
    pub fn review(&self) -> Option<&ReviewSession> {
        self.review.as_ref()
    }

    /// Takes the keys again once a view shown on top of it closes.
    pub fn refocus(&mut self, cx: &mut Context<Self>) {
        self.focus_on_render = true;
        cx.notify();
    }

    fn rebuild_rows(&mut self) {
        self.rows.clear();
        let Content::Loaded(comparison) = &self.content else {
            return;
        };
        for (section, commits, count) in [
            (Section::Ahead, &comparison.ahead, comparison.ahead_count),
            (Section::Behind, &comparison.behind, comparison.behind_count),
        ] {
            self.rows.push(Row::Header(section));
            self.rows
                .extend((0..commits.len()).map(|ix| Row::Commit(section, ix)));
            if count > commits.len() {
                self.rows.push(Row::More(count - commits.len()));
            }
        }
        match self.since() {
            Some(Ok(SinceReview::Advanced { files, .. })) => {
                let count = files.len();
                self.rows.push(Row::Header(Section::Since));
                self.rows.extend((0..count).map(Row::SinceFile));
            }
            Some(Ok(SinceReview::Rewritten { pairs })) => {
                let count = pairs.len();
                self.rows.push(Row::Header(Section::Since));
                self.rows.extend((0..count).map(Row::Pair));
            }
            Some(Err(_)) => self
                .rows
                .extend([Row::Header(Section::Since), Row::SinceNote]),
            Some(Ok(SinceReview::Unchanged)) | None => {}
        }
        let Content::Loaded(comparison) = &self.content else {
            return;
        };
        self.rows.push(Row::Header(Section::Files));
        self.rows.extend((0..comparison.files.len()).map(Row::File));
        if let Some(review) = &self.review
            && !review.comments.is_empty()
        {
            self.rows.push(Row::Header(Section::Comments));
            let mut order: Vec<usize> = (0..review.comments.len()).collect();
            order.sort_by(|&a, &b| {
                let (a, b) = (&review.comments[a], &review.comments[b]);
                (&a.path, a.line).cmp(&(&b.path, b.line))
            });
            self.rows.extend(order.into_iter().map(Row::Comment));
        }
        if let Some(review) = &self.review
            && !review.deleted.is_empty()
        {
            self.rows.push(Row::Header(Section::Deleted));
            self.rows
                .extend((0..review.deleted.len()).map(Row::Deleted));
        }
    }

    /// What changed since the last review, if it was computed for the range the review
    /// holds now (marking it reviewed meanwhile makes an older result stale).
    fn since(&self) -> Option<&Result<SinceReview, String>> {
        let reviewed = self.review.as_ref()?.reviewed?;
        let (computed_for, since) = self.since.as_ref()?;
        (*computed_for == reviewed).then_some(since)
    }

    /// Whether the review was marked done at the compared ref as it is now.
    fn reviewed_now(&self) -> bool {
        match (&self.content, self.review.as_ref().and_then(|r| r.reviewed)) {
            (Content::Loaded(comparison), Some(reviewed)) => reviewed.head == comparison.head,
            _ => false,
        }
    }

    /// Marks the review done at the range shown now: later changes show against it. Every
    /// file is marked viewed too, at its current content (#35: a reviewed range with
    /// unviewed files read as unfinished); a file that changes later is unviewed by itself.
    fn mark_reviewed(&mut self, cx: &mut Context<Self>) {
        let (Content::Loaded(comparison), Some(review)) = (&self.content, &mut self.review) else {
            return;
        };
        let (base, head) = (
            comparison.merge_base.unwrap_or(comparison.base),
            comparison.head,
        );
        review.mark_reviewed(base, head);
        for (change, blob) in comparison.files.iter().zip(&comparison.blobs) {
            review.set_viewed(&path_key(&change.path), *blob, true);
        }
        self.since = review
            .reviewed
            .map(|range| (range, Ok(SinceReview::Unchanged)));
        cx.emit(CompareEvent::ReviewChanged(review.clone()));
        self.rebuild_rows();
        self.selected = None;
        cx.notify();
    }

    /// Hands the review, as Markdown, over to be saved, copied or noted (in the UI's
    /// language: see `export_words`).
    fn export(&mut self, target: ExportTarget, cx: &mut Context<Self>) {
        let (Content::Loaded(comparison), Some(review)) = (&self.content, &self.review) else {
            return;
        };
        let info = ExportInfo {
            head: comparison.head,
            merge_base: comparison.merge_base,
        };
        let markdown = review.to_markdown_in(&info, &export_words());
        let name = format!("review-{}.md", self.head_name.replace('/', "-"));
        cx.emit(CompareEvent::ExportReview {
            markdown,
            name,
            head: comparison.head,
            target,
        });
    }

    /// The comments on `path` that have a place in the current version.
    pub fn comments_for(&self, path: &str) -> Vec<Comment> {
        self.review
            .iter()
            .flat_map(|review| &review.comments)
            .filter(|comment| comment.path == path && !comment.outdated)
            .cloned()
            .collect()
    }

    pub fn comment(&self, id: u64) -> Option<&Comment> {
        self.review
            .iter()
            .flat_map(|review| &review.comments)
            .find(|comment| comment.id == id)
    }

    /// Adds a comment, written against the compared ref as it is now.
    pub fn add_comment(&mut self, draft: CommentDraft, cx: &mut Context<Self>) {
        let (Content::Loaded(comparison), Some(review)) = (&self.content, &mut self.review) else {
            return;
        };
        let created = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_secs());
        review.add_comment(NewComment {
            path: draft.path,
            side: draft.side,
            line: draft.line,
            anchor: draft.anchor,
            body: draft.body,
            author: draft.author,
            commit: comparison.head,
            created,
        });
        self.comments_changed(cx);
    }

    pub fn set_resolved(&mut self, id: u64, resolved: bool, cx: &mut Context<Self>) {
        if let Some(comment) = self
            .review
            .as_mut()
            .and_then(|review| review.comment_mut(id))
        {
            comment.resolved = resolved;
            self.comments_changed(cx);
        }
    }

    pub fn edit_comment(&mut self, id: u64, body: String, cx: &mut Context<Self>) {
        if let Some(comment) = self
            .review
            .as_mut()
            .and_then(|review| review.comment_mut(id))
        {
            comment.body = body;
            self.comments_changed(cx);
        }
    }

    /// Marks every file viewed at its current content.
    fn mark_all_viewed(&mut self, cx: &mut Context<Self>) {
        let (Content::Loaded(comparison), Some(review)) = (&self.content, &mut self.review) else {
            return;
        };
        for (change, blob) in comparison.files.iter().zip(&comparison.blobs) {
            review.set_viewed(&path_key(&change.path), *blob, true);
        }
        cx.emit(CompareEvent::ReviewChanged(review.clone()));
        cx.notify();
    }

    pub fn delete_comment(&mut self, id: u64, cx: &mut Context<Self>) {
        if let Some(review) = &mut self.review {
            review.remove_comment(id);
            self.comments_changed(cx);
        }
    }

    /// Brings a deleted comment back (#35).
    pub fn restore_comment(&mut self, id: u64, cx: &mut Context<Self>) {
        if let Some(review) = &mut self.review
            && review.restore_comment(id)
        {
            self.comments_changed(cx);
        }
    }

    /// Saves the session and redraws the comment list.
    fn comments_changed(&mut self, cx: &mut Context<Self>) {
        if let Some(review) = &self.review {
            cx.emit(CompareEvent::ReviewChanged(review.clone()));
        }
        self.rebuild_rows();
        self.selected = None;
        cx.notify();
    }

    /// The event that opens the file at `index` of the comparison.
    fn open_file(&self, comparison: &Comparison, index: usize) -> CompareEvent {
        CompareEvent::OpenFile {
            from: comparison.merge_base.unwrap_or(comparison.base),
            to: comparison.head,
            change: comparison.files[index].clone(),
            subtitle: t!(
                "compare.diff_subtitle",
                head = self.head_name.clone(),
                base = self.base_name.clone()
            )
            .into_owned(),
            kind: ReviewDiff::Whole,
        }
    }

    /// The event for a row of the "since last review" section.
    fn open_since(&self, comparison: &Comparison, row: Row) -> Option<CompareEvent> {
        let reviewed = self.review.as_ref()?.reviewed?;
        match (self.since()?.as_ref().ok()?, row) {
            (SinceReview::Advanced { from, files, .. }, Row::SinceFile(ix)) => {
                Some(CompareEvent::OpenFile {
                    from: *from,
                    to: comparison.head,
                    change: files.get(ix)?.clone(),
                    subtitle: t!(
                        "review.since_subtitle",
                        head = self.head_name.clone(),
                        sha = reviewed.head.to_hex_with_len(8)
                    )
                    .into_owned(),
                    kind: ReviewDiff::Since,
                })
            }
            (SinceReview::Rewritten { pairs }, Row::Pair(ix)) => {
                let pair = pairs.get(ix)?;
                match (pair.change, pair.old, pair.new) {
                    (RangeChange::Changed, Some((_, old)), Some((_, new))) => {
                        Some(CompareEvent::OpenInterdiff {
                            title: t!("review.interdiff_title", summary = pair.summary.clone())
                                .into_owned(),
                            subtitle: t!(
                                "review.interdiff_subtitle",
                                old = old.to_hex_with_len(8),
                                new = new.to_hex_with_len(8)
                            )
                            .into_owned(),
                            lines: pair.interdiff.clone(),
                            commit: new,
                        })
                    }
                    // Otherwise the commit as it is now, or as it was when removed.
                    (_, old, new) => new.or(old).map(|(_, id)| CompareEvent::SelectCommit(id)),
                }
            }
            _ => None,
        }
    }

    /// Marks the file at `index` viewed (at its current content) or not, and hands the
    /// session over to be saved.
    fn set_viewed(&mut self, index: usize, viewed: bool, cx: &mut Context<Self>) {
        let (Content::Loaded(comparison), Some(review)) = (&self.content, &mut self.review) else {
            return;
        };
        let (Some(change), Some(blob)) = (comparison.files.get(index), comparison.blobs.get(index))
        else {
            return;
        };
        review.set_viewed(&path_key(&change.path), *blob, viewed);
        cx.emit(CompareEvent::ReviewChanged(review.clone()));
        cx.notify();
    }

    /// How many of the files are viewed at their current content, of how many; `None`
    /// without a review.
    fn viewed_count(&self, comparison: &Comparison) -> Option<(usize, usize)> {
        let review = self.review.as_ref()?;
        let viewed = comparison
            .files
            .iter()
            .zip(&comparison.blobs)
            .filter(|(change, blob)| review.is_viewed(&path_key(&change.path), **blob))
            .count();
        Some((viewed, comparison.files.len()))
    }

    fn close(&mut self, _: &CloseCompare, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(CompareEvent::Close);
    }

    fn click(&mut self, row: Row, cx: &mut Context<Self>) {
        let Content::Loaded(comparison) = &self.content else {
            return;
        };
        match row {
            Row::Commit(section, ix) => {
                cx.emit(CompareEvent::SelectCommit(
                    commits(comparison, section)[ix].id,
                ));
            }
            Row::File(ix) => cx.emit(self.open_file(comparison, ix)),
            Row::SinceFile(_) | Row::Pair(_) => match self.open_since(comparison, row) {
                Some(event) => cx.emit(event),
                None => return,
            },
            // A comment opens its file's diff, if the file is still part of the comparison.
            // Otherwise (outdated, or its file no longer changed) the comment itself.
            Row::Comment(ix) => {
                let Some(comment) = self
                    .review
                    .as_ref()
                    .and_then(|review| review.comments.get(ix))
                else {
                    return;
                };
                let file = comparison
                    .files
                    .iter()
                    .position(|change| path_key(&change.path) == comment.path);
                match file {
                    Some(file) if !comment.outdated => cx.emit(self.open_file(comparison, file)),
                    _ => cx.emit(CompareEvent::OpenComment(comment.id)),
                }
            }
            // Restored with its link (or the row menu), not by a click on the row.
            Row::Deleted(_) => {}
            // The rest of a capped list (#44): compared again, uncapped.
            Row::More(_) => {
                self.show_all = true;
                cx.emit(CompareEvent::Recompare);
                return;
            }
            Row::Header(_) | Row::SinceNote => return,
        }
        self.selected = Some(row);
        cx.notify();
    }

    /// The rows the keys stop at: commits, files, comments and what changed since the
    /// review, not headings and notes.
    fn stops_at(row: Row) -> bool {
        !matches!(row, Row::Header(_) | Row::SinceNote)
    }

    /// The arrows move the cursor through the rows that do something and keep it in view
    /// (#27). A commit shows its details, as a click does; a file's diff waits for Enter, so
    /// moving on doesn't open one diff over the list after another.
    fn move_cursor(&mut self, to: Step, cx: &mut Context<Self>) {
        let stops: Vec<usize> = (0..self.rows.len())
            .filter(|&ix| Self::stops_at(self.rows[ix]))
            .collect();
        let current = self
            .selected
            .and_then(|row| stops.iter().position(|&ix| self.rows[ix] == row));
        let Some(stop) = list_keys::step(stops.len(), current, to) else {
            return;
        };
        let (ix, row) = (stops[stop], self.rows[stops[stop]]);
        self.scroll.scroll_to_item(ix, ScrollStrategy::Nearest);
        self.selected = Some(row);
        if let (Row::Commit(section, commit), Content::Loaded(comparison)) = (row, &self.content) {
            cx.emit(CompareEvent::SelectCommit(
                commits(comparison, section)[commit].id,
            ));
        }
        cx.notify();
    }

    /// Whether file `index` is marked viewed at its current content; `None` without a
    /// review (nothing to mark).
    fn viewed(&self, index: usize) -> Option<bool> {
        let review = self.review.as_ref()?;
        let Content::Loaded(comparison) = &self.content else {
            return None;
        };
        let change = comparison.files.get(index)?;
        Some(
            comparison
                .blobs
                .get(index)
                .is_some_and(|blob| review.is_viewed(&path_key(&change.path), *blob)),
        )
    }

    /// Space: ticks or unticks the cursor's file as viewed.
    fn toggle_viewed(&mut self, cx: &mut Context<Self>) {
        let Some(Row::File(ix)) = self.selected else {
            return;
        };
        if let Some(viewed) = self.viewed(ix) {
            self.set_viewed(ix, !viewed, cx);
        }
    }

    /// A commit's or file's menu, for its right-click menu and Shift+F10 (#44): the same
    /// items the graph's and the details' rows have.
    fn row_items(&self, row: Row) -> Vec<(String, CompareEvent)> {
        let Content::Loaded(comparison) = &self.content else {
            return Vec::new();
        };
        match row {
            Row::Commit(section, ix) => {
                let commit = &commits(comparison, section)[ix];
                vec![
                    (
                        t!("history.show_in_graph").into_owned(),
                        CompareEvent::ShowInGraph(commit.id),
                    ),
                    (
                        t!("rewrite.cherry_pick").into_owned(),
                        CompareEvent::CherryPick(commit.id, commit.summary.clone()),
                    ),
                    (
                        t!("branch.new_here").into_owned(),
                        CompareEvent::CreateBranch(commit.id),
                    ),
                ]
            }
            Row::File(ix) => {
                let Some(change) = comparison.files.get(ix) else {
                    return Vec::new();
                };
                let head = comparison.head;
                let mut items = Vec::new();
                // A file the comparison deleted has nothing at the head to blame.
                if change.kind != gitweft_git::read::ChangeKind::Deleted {
                    items.push((
                        t!("blame.open").into_owned(),
                        CompareEvent::Blame {
                            path: change.path.clone(),
                            commit: head,
                        },
                    ));
                }
                items.push((
                    t!("history.open").into_owned(),
                    CompareEvent::History {
                        path: change.path.clone(),
                        commit: head,
                    },
                ));
                items.push((
                    t!("external.diff_tool").into_owned(),
                    CompareEvent::DiffTool {
                        from: comparison.merge_base.unwrap_or(comparison.base),
                        to: head,
                        change: change.clone(),
                    },
                ));
                items
            }
            _ => Vec::new(),
        }
    }

    /// What a row is, for the palette: a commit's id and summary, a file's path, a
    /// comment's place.
    fn row_detail(&self, row: Row) -> String {
        let Content::Loaded(comparison) = &self.content else {
            return String::new();
        };
        match row {
            Row::Commit(section, ix) => {
                let commit = &commits(comparison, section)[ix];
                format!("{} {}", commit.id.to_hex_with_len(8), commit.summary)
            }
            Row::File(ix) => comparison
                .files
                .get(ix)
                .map(|change| change.path.to_string())
                .unwrap_or_default(),
            Row::SinceFile(ix) => match self.since() {
                Some(Ok(SinceReview::Advanced { files, .. })) => files
                    .get(ix)
                    .map(|change| change.path.to_string())
                    .unwrap_or_default(),
                _ => String::new(),
            },
            Row::Comment(ix) => self
                .review
                .as_ref()
                .and_then(|review| review.comments.get(ix))
                .map(|comment| format!("{}:{}", path_shown(&comment.path), comment.line))
                .unwrap_or_default(),
            Row::Deleted(ix) => self
                .review
                .as_ref()
                .and_then(|review| review.deleted.get(ix))
                .map(|comment| format!("{}:{}", path_shown(&comment.path), comment.line))
                .unwrap_or_default(),
            Row::Pair(_) | Row::Header(_) | Row::More(_) | Row::SinceNote => String::new(),
        }
    }

    /// Shift+F10: the cursor row's actions in the palette: open it (as a click), and a
    /// file's Viewed.
    fn row_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.selected else {
            return;
        };
        let this = cx.entity().downgrade();
        let group: SharedString = t!("palette.group.compare").into_owned().into();
        let detail = self.row_detail(row);
        let open = view_command(
            &this,
            &group,
            t!("palette.compare_open"),
            move |this, _, cx| this.click(row, cx),
        );
        let mut commands = vec![open.detail(detail.clone())];
        if let Row::File(ix) = row
            && let Some(viewed) = self.viewed(ix)
        {
            let label = if viewed {
                t!("palette.compare_not_viewed")
            } else {
                t!("palette.compare_viewed")
            };
            let toggle = view_command(&this, &group, label, move |this, _, cx| {
                this.set_viewed(ix, !viewed, cx)
            });
            commands.push(toggle.detail(detail.clone()));
        }
        commands.extend(list_keys::event_commands(
            &this,
            &group,
            &detail,
            self.row_items(row),
        ));
        crate::palette::open(commands, window, cx);
    }

    fn header_text(&self, section: Section, comparison: &Comparison) -> String {
        match section {
            Section::Ahead => t!(
                "compare.only_in",
                name = self.head_name.clone(),
                count = comparison.ahead_count
            ),
            Section::Behind => t!(
                "compare.only_in",
                name = self.base_name.clone(),
                count = comparison.behind_count
            ),
            Section::Files => t!(
                "compare.files",
                name = self.head_name.clone(),
                count = comparison.files.len()
            ),
            Section::Comments => t!(
                "review.comments",
                count = self
                    .review
                    .as_ref()
                    .map_or(0, |review| review.comments.len())
            ),
            Section::Deleted => tn!(
                "review.deleted_header",
                count = self
                    .review
                    .as_ref()
                    .map_or(0, |review| review.deleted.len())
            ),
            Section::Since => {
                let sha = self
                    .review
                    .as_ref()
                    .and_then(|review| review.reviewed)
                    .map(|reviewed| reviewed.head.to_hex_with_len(8).to_string())
                    .unwrap_or_default();
                match self.since() {
                    Some(Ok(SinceReview::Advanced { commits, .. })) => {
                        tn!("review.since", count = *commits, sha = sha)
                    }
                    Some(Ok(SinceReview::Rewritten { .. })) => t!("review.rewritten", sha = sha),
                    _ => t!("review.since_failed_header", sha = sha),
                }
            }
        }
        .into_owned()
    }

    fn render_rows(
        &mut self,
        range: Range<usize>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Content::Loaded(comparison) = &self.content else {
            return Vec::new();
        };
        let look = cx.look().clone();
        let (p, m) = (look.palette, look.metrics);
        let view = cx.entity().downgrade();
        range
            .map(|ix| {
                let row = self.rows[ix];
                let id = ("compare-row", ix);
                let selected = self.selected == Some(row);
                match row {
                    Row::Header(section) => {
                        let all_viewed = self
                            .viewed_count(comparison)
                            .is_none_or(|(viewed, total)| viewed == total);
                        let rewritten =
                            matches!(self.since(), Some(Ok(SinceReview::Rewritten { .. })));
                        plain_row(&m)
                            .bg(p.panel2)
                            .text_color(p.fg)
                            .child(
                                truncated("header", self.header_text(section, comparison))
                                    .flex_1()
                                    .font_semibold(),
                            )
                            .when(section == Section::Files && !all_viewed, |this| {
                                this.child(link(
                                    "mark-all-viewed",
                                    t!("review.mark_all_viewed").into_owned(),
                                    p.acc,
                                    m.text.small,
                                    cx.listener(|this, _, _, cx| this.mark_all_viewed(cx)),
                                ))
                            })
                            // A heavily reworked commit shows as removed plus added unless
                            // matched harder.
                            .when(section == Section::Since && rewritten, |this| {
                                this.child(
                                    Checkbox::new("match-harder")
                                        .small()
                                        .label(t!("review.match_harder").into_owned())
                                        .checked(self.match_harder)
                                        .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                            this.match_harder = *checked;
                                            cx.emit(CompareEvent::Recompare);
                                            cx.notify();
                                        })),
                                )
                            })
                            .into_any_element()
                    }
                    // A stop for the keys too: Enter shows them all, as a click does.
                    Row::More(count) => list_row(id, selected, &p, &m)
                        .on_click(cx.listener(move |this, _, _, cx| this.click(row, cx)))
                        .text_size(m.text.xs)
                        .text_color(p.fg3)
                        .child(tn!("compare.more_show_all", count = count).into_owned())
                        .into_any_element(),
                    Row::Commit(section, commit_ix) => {
                        let commit = &commits(comparison, section)[commit_ix];
                        let cells = commit_columns::cells(
                            CommitRow {
                                summary: commit.summary.clone(),
                                note: None,
                                author: commit.author.name.clone(),
                                date: format_time(commit.author.time),
                                id: commit.id,
                            },
                            &p,
                            &m,
                        );
                        let (view, items) = (view.clone(), self.row_items(row));
                        list_row(id, selected, &p, &m)
                            .on_click(cx.listener(move |this, _, _, cx| this.click(row, cx)))
                            .children(cells)
                            .context_menu(move |menu, _, _| {
                                list_keys::event_menu(menu, &view, &items)
                            })
                            .into_any_element()
                    }
                    Row::File(file_ix) => {
                        let change = &comparison.files[file_ix];
                        let (letter, color) = change_style(change.kind, &p);
                        let path = match &change.old_path {
                            Some(old) => format!("{old} → {}", change.path),
                            None => change.path.to_string(),
                        };
                        let viewed = self.viewed(file_ix);
                        list_row(id, selected, &p, &m)
                            .on_click(cx.listener(move |this, _, _, cx| this.click(row, cx)))
                            .child(status_badge(letter, color, &m))
                            // A viewed file's path is dimmed.
                            .child(
                                path_label(&path, &p, &m)
                                    .when(viewed == Some(true), |this| this.opacity(p.dim)),
                            )
                            // The checkbox keeps its click, so ticking it doesn't open the diff.
                            .when_some(viewed, |this, viewed| {
                                this.child(
                                    Checkbox::new(("viewed", file_ix))
                                        .small()
                                        .label(t!("review.viewed").into_owned())
                                        .checked(viewed)
                                        .on_click(cx.listener(
                                            move |this, checked: &bool, _, cx| {
                                                cx.stop_propagation();
                                                this.set_viewed(file_ix, *checked, cx);
                                            },
                                        )),
                                )
                            })
                            .context_menu({
                                let (view, items) = (view.clone(), self.row_items(row));
                                move |menu, _, _| list_keys::event_menu(menu, &view, &items)
                            })
                            .into_any_element()
                    }
                    Row::SinceFile(file_ix) => {
                        let Some(Ok(SinceReview::Advanced { files, .. })) = self.since() else {
                            return plain_row(&m).into_any_element();
                        };
                        let Some(change) = files.get(file_ix) else {
                            return plain_row(&m).into_any_element();
                        };
                        let (letter, color) = change_style(change.kind, &p);
                        let path = match &change.old_path {
                            Some(old) => format!("{old} → {}", change.path),
                            None => change.path.to_string(),
                        };
                        list_row(id, selected, &p, &m)
                            .on_click(cx.listener(move |this, _, _, cx| this.click(row, cx)))
                            .child(status_badge(letter, color, &m))
                            .child(path_label(&path, &p, &m))
                            .into_any_element()
                    }
                    Row::Pair(pair_ix) => {
                        let Some(Ok(SinceReview::Rewritten { pairs })) = self.since() else {
                            return plain_row(&m).into_any_element();
                        };
                        let Some(pair) = pairs.get(pair_ix) else {
                            return plain_row(&m).into_any_element();
                        };
                        list_row(id, selected, &p, &m)
                            .on_click(cx.listener(move |this, _, _, cx| this.click(row, cx)))
                            .children(pair_cells(pair, &p, &m))
                            .into_any_element()
                    }
                    Row::SinceNote => {
                        let text = match self.since() {
                            Some(Err(message)) => message.clone(),
                            _ => String::new(),
                        };
                        plain_row(&m)
                            .text_size(m.text.xs)
                            .text_color(p.del)
                            .child(truncated("failure", text))
                            .into_any_element()
                    }
                    Row::Comment(comment_ix) => {
                        let Some(comment) = self
                            .review
                            .as_ref()
                            .and_then(|review| review.comments.get(comment_ix))
                        else {
                            return plain_row(&m).into_any_element();
                        };
                        let place = format!("{}:{}", path_shown(&comment.path), comment.line);
                        let status = if comment.outdated {
                            Some(t!("review.outdated"))
                        } else if comment.resolved {
                            Some(t!("review.resolved"))
                        } else {
                            None
                        };
                        let dim = comment.outdated || comment.resolved;
                        list_row(id, selected, &p, &m)
                            .on_click(cx.listener(move |this, _, _, cx| this.click(row, cx)))
                            .child(
                                truncated("place", place)
                                    .w(px(PLACE_WIDTH * m.zoom))
                                    .flex_shrink_0()
                                    .font_family(mono_font())
                                    .text_size(m.text.xs)
                                    .text_color(p.fg3),
                            )
                            .child(
                                truncated("comment", crate::diff_view::first_line(&comment.body))
                                    .flex_1()
                                    .text_color(if dim { p.fg3 } else { p.fg }),
                            )
                            .when_some(status, |this, status| {
                                this.child(
                                    div()
                                        .flex_shrink_0()
                                        .text_size(m.text.xs)
                                        .text_color(p.fg3)
                                        .child(status.into_owned()),
                                )
                            })
                            .into_any_element()
                    }
                    Row::Deleted(deleted_ix) => {
                        let Some(comment) = self
                            .review
                            .as_ref()
                            .and_then(|review| review.deleted.get(deleted_ix))
                        else {
                            return plain_row(&m).into_any_element();
                        };
                        let comment_id = comment.id;
                        let place = format!("{}:{}", path_shown(&comment.path), comment.line);
                        list_row(id, selected, &p, &m)
                            .on_click(cx.listener(move |this, _, _, cx| this.click(row, cx)))
                            .child(
                                truncated("place", place)
                                    .w(px(PLACE_WIDTH * m.zoom))
                                    .flex_shrink_0()
                                    .font_family(mono_font())
                                    .text_size(m.text.xs)
                                    .text_color(p.fg3),
                            )
                            .child(
                                truncated("comment", crate::diff_view::first_line(&comment.body))
                                    .flex_1()
                                    .text_color(p.fg3),
                            )
                            .child(link(
                                ("restore-comment", deleted_ix),
                                t!("review.restore").into_owned(),
                                p.acc,
                                m.text.small,
                                cx.listener(move |this, _, _, cx| {
                                    this.restore_comment(comment_id, cx)
                                }),
                            ))
                            .into_any_element()
                    }
                }
            })
            .collect()
    }
}

fn commits(comparison: &Comparison, section: Section) -> &[ComparedCommit] {
    match section {
        Section::Behind => &comparison.behind,
        Section::Ahead | Section::Files | Section::Comments | Section::Deleted | Section::Since => {
            &comparison.ahead
        }
    }
}

/// A row that doesn't click (a section header, "… and N more", the since-note), with a
/// list row's height, padding and gaps: the uniform list needs every row to be one height.
fn plain_row(m: &Metrics) -> Div {
    let z = |value: f32| px(value * m.zoom);
    h_flex()
        .h(m.row)
        .w_full()
        .flex_shrink_0()
        .overflow_hidden()
        .px(z(12.))
        .gap(z(12.))
}

/// A range-diff pair's word and colour: same, changed, removed or added.
fn pair_style(change: RangeChange, p: &Palette) -> (String, Hsla) {
    let (label, color) = match change {
        RangeChange::Same => (t!("review.pair_same"), p.fg3),
        RangeChange::Changed => (t!("review.pair_changed"), p.modified),
        RangeChange::Removed => (t!("review.pair_removed"), p.del),
        RangeChange::Added => (t!("review.pair_added"), p.add),
    };
    (label.into_owned(), color)
}

/// The old commit (position and id), what happened to it, the new one, and the summary.
fn pair_cells(pair: &RangePair, p: &Palette, m: &Metrics) -> [AnyElement; 4] {
    let (label, color) = pair_style(pair.change, p);
    let side = |side: Option<(usize, ObjectId)>| match side {
        Some((position, id)) => format!("{position}: {}", id.to_hex_with_len(8)),
        None => "-".to_owned(),
    };
    let id_cell = |id: &'static str, text: String| {
        truncated(id, text)
            .w(px(PAIR_ID_WIDTH * m.zoom))
            .flex_shrink_0()
            .font_family(mono_font())
            .text_size(m.text.mono)
            .text_color(p.fg3)
            .into_any_element()
    };
    let removed = pair.change == RangeChange::Removed;
    [
        id_cell("old", side(pair.old)),
        truncated("label", label)
            .w(px(PAIR_LABEL_WIDTH * m.zoom))
            .flex_shrink_0()
            .text_size(m.text.xs)
            .font_semibold()
            .text_color(color)
            .into_any_element(),
        id_cell("new", side(pair.new)),
        truncated("summary", pair.summary.clone())
            .flex_1()
            .text_color(if removed { p.fg3 } else { p.fg })
            .into_any_element(),
    ]
}

impl Render for CompareView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.focus_on_render) {
            self.focus.focus(window, cx);
        }
        let look = cx.look().clone();
        let (p, m) = (look.palette, look.metrics);

        let summary = match &self.content {
            Content::Loaded(comparison) => {
                let counts = t!(
                    "compare.counts",
                    ahead = tn!("compare.ahead", count = comparison.ahead_count),
                    behind = tn!("compare.behind", count = comparison.behind_count)
                );
                let base = match comparison.merge_base {
                    Some(merge_base) => {
                        t!("compare.merge_base", sha = merge_base.to_hex_with_len(8))
                    }
                    None => t!("compare.unrelated"),
                };
                let mut summary = t!("common.middle_dot", first = counts, second = base);
                if let Some((viewed, total)) = self.viewed_count(comparison) {
                    let viewed = t!("review.viewed_count", viewed = viewed, total = total);
                    summary = t!("common.middle_dot", first = summary, second = viewed);
                }
                if let Some(reviewed) = self.review.as_ref().and_then(|review| review.reviewed) {
                    let at = t!("review.reviewed_at", sha = reviewed.head.to_hex_with_len(8));
                    summary = t!("common.middle_dot", first = summary, second = at);
                }
                summary.into_owned()
            }
            Content::Loading | Content::Failed(_) => String::new(),
        };
        let title = t!(
            "compare.title",
            head = self.head_name.clone(),
            base = self.base_name.clone()
        )
        .into_owned();
        let header = centre_header(title, summary, &p, &m)
            .when(
                self.review
                    .as_ref()
                    .is_some_and(|review| !review.comments.is_empty())
                    && matches!(self.content, Content::Loaded(_)),
                |this| {
                    this.child(
                        Button::new("export-review")
                            .small()
                            .ghost()
                            .label(t!("review.export").into_owned())
                            .dropdown_menu_with_anchor(gpui_kit::Anchor::TopRight, {
                                let view = cx.entity().downgrade();
                                move |menu, _, _| {
                                    let item = |label: String, target: ExportTarget| {
                                        let view = view.clone();
                                        PopupMenuItem::new(label).on_click(move |_, _, cx| {
                                            view.update(cx, |this, cx| this.export(target, cx))
                                                .ok();
                                        })
                                    };
                                    menu.item(item(
                                        t!("review.export_file").into_owned(),
                                        ExportTarget::File,
                                    ))
                                    .item(item(
                                        t!("review.export_clipboard").into_owned(),
                                        ExportTarget::Clipboard,
                                    ))
                                    .item(item(
                                        t!("review.export_note").into_owned(),
                                        ExportTarget::Note,
                                    ))
                                }
                            }),
                    )
                },
            )
            // A review can be marked done at the version shown; later changes show against it.
            .when(
                self.review.is_some() && matches!(self.content, Content::Loaded(_)),
                |this| {
                    let done = self.reviewed_now();
                    this.child(
                        Button::new("mark-reviewed")
                            .debug_selector(|| "mark-reviewed".into())
                            .small()
                            .when(!done, |this| this.primary())
                            .when(done, |this| this.ghost())
                            .disabled(done)
                            .label(if done {
                                t!("review.reviewed_done").into_owned()
                            } else {
                                t!("review.mark_reviewed").into_owned()
                            })
                            .tooltip(t!("review.mark_reviewed_tip").into_owned())
                            .on_click(cx.listener(|this, _, _, cx| this.mark_reviewed(cx))),
                    )
                },
            )
            .child(
                Button::new("swap-compare")
                    .small()
                    .ghost()
                    .label(t!("compare.swap").into_owned())
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(CompareEvent::Swap))),
            )
            .child(
                Button::new("close-compare")
                    .small()
                    .ghost()
                    .label(t!("diff.close").into_owned())
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(CompareEvent::Close))),
            );

        let body = match &self.content {
            Content::Loading => {
                placeholder(t!("repo.loading").into_owned(), p.fg3, &m).into_any_element()
            }
            Content::Failed(message) => placeholder(message.clone(), p.del, &m).into_any_element(),
            Content::Loaded(_) => uniform_list(
                "compare-rows",
                self.rows.len(),
                cx.processor(Self::render_rows),
            )
            .track_scroll(&self.scroll)
            .flex_1()
            .into_any_element(),
        };

        v_flex()
            .size_full()
            .text_size(m.text.ui)
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::close))
            .on_action(cx.listener(|this, _: &NextRow, _, cx| this.move_cursor(Step::Next, cx)))
            .on_action(
                cx.listener(|this, _: &PreviousRow, _, cx| this.move_cursor(Step::Previous, cx)),
            )
            .on_action(cx.listener(|this, _: &FirstRow, _, cx| this.move_cursor(Step::First, cx)))
            .on_action(cx.listener(|this, _: &LastRow, _, cx| this.move_cursor(Step::Last, cx)))
            .on_action(cx.listener(|this, _: &OpenRow, _, cx| {
                if let Some(row) = this.selected {
                    this.click(row, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleRow, _, cx| this.toggle_viewed(cx)))
            .on_action(cx.listener(|this, _: &RowMenu, window, cx| this.row_menu(window, cx)))
            .child(header)
            .child(body)
    }
}

/// The export's phrases in the UI's language, placeholders left for the export to fill.
fn export_words() -> ExportWords {
    let text = |key: &str| t!(key).into_owned();
    ExportWords {
        title: text("review.md_title"),
        against: text("review.md_against"),
        branch_at: text("review.md_branch_at"),
        merge_base: text("review.md_merge_base"),
        reviewed_at: text("review.md_reviewed_at"),
        place: text("review.md_place"),
        old: text("review.md_old"),
        new: text("review.md_new"),
        resolved: text("review.md_resolved"),
        outdated: text("review.md_outdated"),
        no_comments: text("review.md_no_comments"),
        by: text("review.md_by"),
    }
}
