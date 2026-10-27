//! The command palette (Ctrl+Shift+P): the commands of the window and of the active tab in
//! one list, found by typing a few letters of them in any order of words.
//!
//! The kit's [`Command`] draws the list and handles the keys; its own filter (a substring
//! test) is off, and [`rank`] scores the commands instead (a subsequence per word, with
//! bonuses for word starts and runs, as fuzzy finders do).

use std::ops::Range;
use std::rc::Rc;

use gitweft_ui::truncated::truncated;
use gitweft_ui::{LookExt, Metrics, Palette};
use gpui_kit::component::command::{Command, CommandGroup, CommandItem, CommandState};
use gpui_kit::component::kbd::Kbd;
use gpui_kit::component::{IndexPath, WindowExt as _, h_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

/// The palette's width and its list's greatest height, in design pixels.
const WIDTH: f32 = 560.;
const LIST_HEIGHT: f32 = 420.;
/// How far below the window's top the palette opens.
const TOP: f32 = 72.;
/// The most commands a group holds while nothing is typed, and the most matches listed
/// after typing. The kit draws only the rows on screen, but every listed command is built
/// again each frame, and a rebase of thousands of commits offers several per commit.
/// Typing finds the rest; the footer says how many are left out.
const MAX_SHOWN: usize = 300;

/// What a command does once chosen (the palette has closed by then).
pub(crate) type Run = Rc<dyn Fn(&mut Window, &mut App)>;

/// One entry of the palette.
pub(crate) struct PaletteCommand {
    /// The heading it is listed under while nothing is typed; groups keep the order in
    /// which their first command came.
    group: SharedString,
    label: String,
    /// Dimmed after the label and searched too: a commit, a branch, a folder.
    detail: Option<String>,
    /// The action whose key binding is shown, and the key context it is bound in.
    key: Option<(Box<dyn Action>, Option<&'static str>)>,
    run: Run,
}

impl PaletteCommand {
    pub(crate) fn new(
        group: impl Into<SharedString>,
        label: impl Into<String>,
        run: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            group: group.into(),
            label: label.into(),
            detail: None,
            key: None,
            run: Rc::new(run),
        }
    }

    #[cfg(test)]
    pub(crate) fn label(&self) -> &str {
        &self.label
    }

    /// Whether it acts on something named after its label (a row, a file, a commit).
    #[cfg(test)]
    pub(crate) fn has_detail(&self) -> bool {
        self.detail.is_some()
    }

    /// The action whose binding it shows, and its key context.
    #[cfg(test)]
    pub(crate) fn shown_key(&self) -> Option<(&dyn Action, Option<&'static str>)> {
        self.key
            .as_ref()
            .map(|(action, context)| (action.as_ref(), *context))
    }

    pub(crate) fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    /// Shows the key bound to `action` (in `context`, or anywhere).
    pub(crate) fn key(self, action: impl Action, context: Option<&'static str>) -> Self {
        self.key_boxed(Box::new(action), context)
    }

    /// [`Self::key`] for an action already boxed.
    pub(crate) fn key_boxed(
        mut self,
        action: Box<dyn Action>,
        context: Option<&'static str>,
    ) -> Self {
        self.key = Some((action, context));
        self
    }
}

/// A command that runs `run` on `view`: a view offering its own buttons in the palette
/// (the conflict tool, the rebase editor, a diff).
pub(crate) fn view_command<V: 'static>(
    view: &WeakEntity<V>,
    group: &SharedString,
    label: impl Into<String>,
    run: impl Fn(&mut V, &mut Window, &mut Context<V>) + 'static,
) -> PaletteCommand {
    let view = view.clone();
    PaletteCommand::new(group.clone(), label, move |window, cx| {
        view.update(cx, |view, cx| run(view, window, cx)).ok();
    })
}

// ---- matching --------------------------------------------------------------------

/// Points for a matched character, and its bonuses.
const MATCH: i32 = 16;
const WORD_START: i32 = 10;
const TEXT_START: i32 = 6;
const RUN: i32 = 10;
/// Taken for each character skipped between two matched ones.
const GAP: i32 = 1;
/// Taken when a match lands inside a word after a skip (or starts there): letters
/// scattered through a long path shouldn't count as a match.
const MID_WORD: i32 = 8;
/// Characters skipped before the first match cost at most this.
const LEADING_GAP: i32 = 8;
/// A word found in the detail instead of the label counts for less.
const IN_DETAIL: i32 = 12;
/// A general command (no detail) over the same action on one of many rows: it makes up for
/// the most a later start in the label costs (`LEADING_GAP`), so "reflog" finds "Show the
/// reflog of HEAD" before each branch's "Show reflog".
const GENERAL: i32 = LEADING_GAP * GAP;
const NONE: i32 = -1_000_000_000;

/// How a command matched the query: its score and the matched characters (byte offsets) in
/// its label and detail, for highlighting.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct Match {
    pub score: i32,
    pub label: Vec<usize>,
    pub detail: Vec<usize>,
}

fn lower(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// Finds `term` (lower-case) in `text` as a subsequence, ignoring case, and returns the
/// best alignment's score and matched byte offsets. Matches at word starts and in runs
/// score higher; skipped characters cost a little.
pub(crate) fn fuzzy(term: &[char], text: &str) -> Option<(i32, Vec<usize>)> {
    if term.is_empty() {
        return Some((0, Vec::new()));
    }
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let (n, m) = (term.len(), chars.len());
    if n > m {
        return None;
    }
    let folded: Vec<char> = chars.iter().map(|&(_, c)| lower(c)).collect();
    let points: Vec<i32> = (0..m)
        .map(|j| {
            let c = chars[j].1;
            let start = match j.checked_sub(1).map(|k| chars[k].1) {
                None => WORD_START + TEXT_START,
                Some(prev) if !prev.is_alphanumeric() => WORD_START,
                Some(prev) if prev.is_lowercase() && c.is_uppercase() => WORD_START,
                Some(_) => 0,
            };
            MATCH + start
        })
        .collect();
    // score[i * m + j]: the best score with term[i] matched at text[j]; from: where
    // term[i - 1] was matched then.
    let mut score = vec![NONE; n * m];
    let mut from = vec![0usize; n * m];
    for i in 0..n {
        // The best match of term[i - 1] at least two characters back, less the gaps since.
        let (mut carry, mut carry_at) = (NONE, 0);
        for j in i..m {
            if i > 0 && j >= 2 {
                carry -= GAP;
                let earlier = score[(i - 1) * m + j - 2] - GAP;
                if earlier > carry {
                    (carry, carry_at) = (earlier, j - 2);
                }
            }
            if folded[j] != term[i] {
                continue;
            }
            let jump = if points[j] == MATCH { MID_WORD } else { 0 };
            let (best, at) = if i == 0 {
                (-(j as i32).min(LEADING_GAP) * GAP - jump, 0)
            } else {
                let previous = score[(i - 1) * m + j - 1];
                let run = if previous > NONE / 2 {
                    previous + RUN
                } else {
                    NONE
                };
                if run >= carry - jump {
                    (run, j - 1)
                } else {
                    (carry - jump, carry_at)
                }
            };
            if best > NONE / 2 {
                score[i * m + j] = best + points[j];
                from[i * m + j] = at;
            }
        }
    }
    let last = (n - 1) * m;
    let (mut j, best) = (0..m)
        .map(|j| (j, score[last + j]))
        .max_by_key(|&(j, s)| (s, std::cmp::Reverse(j)))?;
    // The bonuses must at least make up for the skips and jumps.
    if best < MATCH * n as i32 {
        return None;
    }
    let mut matched = vec![chars[j].0];
    for i in (1..n).rev() {
        j = from[i * m + j];
        matched.push(chars[j].0);
    }
    matched.reverse();
    Some((best, matched))
}

/// Scores a command: every word of the query must be found in its label or its detail.
pub(crate) fn score(terms: &[Vec<char>], label: &str, detail: Option<&str>) -> Option<Match> {
    let mut found = Match::default();
    for term in terms {
        let in_label = fuzzy(term, label);
        let in_detail = detail
            .and_then(|detail| fuzzy(term, detail))
            .map(|(score, at)| (score - IN_DETAIL, at));
        let (score, at, into_detail) = match (in_label, in_detail) {
            (Some(l), Some(d)) if d.0 > l.0 => (d.0, d.1, true),
            (Some((score, at)), _) => (score, at, false),
            (None, Some((score, at))) => (score, at, true),
            (None, None) => return None,
        };
        found.score += score;
        if into_detail {
            found.detail.extend(at);
        } else {
            found.label.extend(at);
        }
    }
    for offsets in [&mut found.label, &mut found.detail] {
        offsets.sort_unstable();
        offsets.dedup();
    }
    Some(found)
}

/// The commands `query` finds, best first: those with every word in the label before those
/// that needed the detail for one ("stage lines" is the selected lines' Stage, not the
/// Stage of a file called lines.txt), then by score (on a tie a command without a detail,
/// which is a general one, before one acting on a row or file; then the shorter label, then
/// the given order); every command, unscored, when the query is blank.
pub(crate) fn rank<'a>(
    query: &str,
    commands: impl IntoIterator<Item = (&'a str, Option<&'a str>)>,
) -> Vec<(usize, Match)> {
    let terms: Vec<Vec<char>> = query
        .split_whitespace()
        .map(|word| word.chars().map(lower).collect())
        .collect();
    let mut found: Vec<(usize, Match, (bool, usize))> = commands
        .into_iter()
        .enumerate()
        .filter_map(|(ix, (label, detail))| {
            score(&terms, label, detail).map(|mut matched| {
                if detail.is_none() {
                    matched.score += GENERAL;
                }
                (ix, matched, (detail.is_some(), label.len()))
            })
        })
        .collect();
    if !terms.is_empty() {
        found.sort_by_key(|(ix, matched, tie)| {
            let in_detail = !matched.detail.is_empty();
            (in_detail, std::cmp::Reverse(matched.score), *tie, *ix)
        });
    }
    found
        .into_iter()
        .map(|(ix, matched, _)| (ix, matched))
        .collect()
}

// ---- the view ----------------------------------------------------------------------

/// The palette in its dialog: the commands, their keys, and what the query shows.
pub(crate) struct CommandPalette {
    state: Entity<CommandState>,
    commands: Vec<PaletteCommand>,
    keys: Vec<Option<Kbd>>,
    /// Headed groups while nothing is typed, else one list of matches, best first.
    shown: Vec<Section>,
    /// The commands left out of `shown` ([`MAX_SHOWN`]).
    hidden: usize,
}

/// A group of the list: its heading, and its commands (index and match).
type Section = (Option<SharedString>, Vec<(usize, Match)>);

/// The list for `ranked` commands: when nothing is typed (`blank`), headed groups in the
/// order of their first command, each cut at [`MAX_SHOWN`] (so one huge group leaves the
/// others listed); else one list of the best [`MAX_SHOWN`] matches. With the number of
/// commands left out.
fn sections<'a>(
    blank: bool,
    mut ranked: Vec<(usize, Match)>,
    group_of: impl Fn(usize) -> &'a SharedString,
) -> (Vec<Section>, usize) {
    if !blank {
        let hidden = ranked.len().saturating_sub(MAX_SHOWN);
        ranked.truncate(MAX_SHOWN);
        return (vec![(None, ranked)], hidden);
    }
    let mut groups: Vec<Section> = Vec::new();
    for (ix, matched) in ranked {
        let heading = group_of(ix);
        match groups
            .iter_mut()
            .find(|(group, _)| group.as_ref() == Some(heading))
        {
            Some((_, items)) => items.push((ix, matched)),
            None => groups.push((Some(heading.clone()), vec![(ix, matched)])),
        }
    }
    let mut hidden = 0;
    for (_, items) in &mut groups {
        hidden += items.len().saturating_sub(MAX_SHOWN);
        items.truncate(MAX_SHOWN);
    }
    (groups, hidden)
}

impl CommandPalette {
    fn new(
        commands: Vec<PaletteCommand>,
        keys: Vec<Option<Kbd>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            state: cx.new(|cx| CommandState::new(window, cx)),
            commands,
            keys,
            shown: Vec::new(),
            hidden: 0,
        };
        this.filter("");
        this
    }

    fn filter(&mut self, query: &str) {
        let ranked = rank(
            query,
            self.commands
                .iter()
                .map(|command| (command.label.as_str(), command.detail.as_deref())),
        );
        let commands = &self.commands;
        (self.shown, self.hidden) =
            sections(query.trim().is_empty(), ranked, |ix| &commands[ix].group);
    }

    fn command_at(&self, at: IndexPath) -> Option<Run> {
        let (ix, _) = self.shown.get(at.section)?.1.get(at.row)?;
        Some(self.commands[*ix].run.clone())
    }

    fn item(&self, ix: usize, matched: &Match, p: &Palette, m: &Metrics) -> CommandItem {
        let command = &self.commands[ix];
        let label = command.label.clone();
        let detail = command.detail.clone();
        let key = self.keys[ix].clone();
        let (label_at, detail_at) = (matched.label.clone(), matched.detail.clone());
        let (fg, fg3, acc, small) = (p.fg, p.fg3, p.acc, m.text.small);
        let m = *m;
        let text = label.clone();
        CommandItem::new().label(label).child(move |_, _| {
            h_flex()
                .w_full()
                .min_w_0()
                .gap(m.dp(8.))
                .child(
                    div()
                        .flex_shrink_0()
                        .text_color(fg)
                        .child(highlighted(&text, &label_at, acc)),
                )
                .when_some(detail.clone(), |row, detail| {
                    let highlights = highlight_ranges(&detail, &detail_at, acc);
                    row.child(
                        truncated("detail", detail)
                            .highlights(highlights)
                            .text_size(small)
                            .text_color(fg3),
                    )
                })
                .child(div().flex_1())
                .when_some(key.clone(), |row, key| row.child(key))
        })
    }
}

/// `text` with the characters at `offsets` in the accent color.
fn highlighted(text: &str, offsets: &[usize], color: Hsla) -> StyledText {
    StyledText::new(text.to_owned()).with_highlights(highlight_ranges(text, offsets, color))
}

/// The characters at `offsets` in `text`, to draw in the accent color.
fn highlight_ranges(
    text: &str,
    offsets: &[usize],
    color: Hsla,
) -> Vec<(Range<usize>, HighlightStyle)> {
    let style = HighlightStyle {
        color: Some(color),
        font_weight: Some(FontWeight::SEMIBOLD),
        ..HighlightStyle::default()
    };
    offsets
        .iter()
        .filter_map(|&at| {
            let len = text.get(at..)?.chars().next()?.len_utf8();
            Some((at..at + len, style))
        })
        .collect()
}

impl Render for CommandPalette {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let look = cx.look().clone();
        let (p, m) = (look.palette, look.metrics);
        let (on_query, on_confirm) = (cx.entity(), cx.entity());
        let (fg3, small, hidden) = (p.fg3, m.text.small, self.hidden);
        let mut command = Command::new(&self.state)
            .filterable(false)
            .bordered(false)
            .max_h(px(LIST_HEIGHT * m.zoom))
            .placeholder(t!("palette.placeholder").into_owned())
            .empty(move |_, _, _| {
                div()
                    .p(m.dp(12.))
                    .text_color(fg3)
                    .child(t!("palette.empty").into_owned())
            })
            // What MAX_SHOWN left out is there to be typed for.
            .when(hidden > 0, |command| {
                command.footer(move |_, _, _| {
                    div()
                        .px(m.dp(12.))
                        .py(m.dp(6.))
                        .text_size(small)
                        .text_color(fg3)
                        .child(tn!("palette.more", count = hidden).into_owned())
                })
            })
            .on_query(move |query, _, cx| {
                on_query.update(cx, |palette, cx| {
                    palette.filter(query);
                    cx.notify();
                })
            })
            .on_confirm(move |at, window, cx| {
                if let Some(run) = on_confirm.read(cx).command_at(at) {
                    window.close_dialog(cx);
                    run(window, cx);
                }
            });
        for (heading, items) in &self.shown {
            let group = heading.iter().fold(CommandGroup::new(), |group, heading| {
                group.label(heading.clone())
            });
            command = command.group(
                group.items(
                    items
                        .iter()
                        .map(|(ix, matched)| self.item(*ix, matched, &p, &m)),
                ),
            );
        }
        div().w_full().child(command)
    }
}

/// Opens the palette over the window with `commands`, the query field focused.
pub(crate) fn open(commands: Vec<PaletteCommand>, window: &mut Window, cx: &mut App) {
    let keys = commands
        .iter()
        .map(|command| {
            let (action, context) = command.key.as_ref()?;
            Kbd::binding_for_action(action.as_ref(), *context, window)
        })
        .collect();
    let palette = cx.new(|cx| CommandPalette::new(commands, keys, window, cx));
    let state = palette.read(cx).state.clone();
    let (bg, zoom) = (cx.palette().panel, cx.metrics().zoom);
    window.open_dialog(cx, move |dialog, _, _| {
        dialog
            .bg(bg)
            .w(px(WIDTH * zoom))
            .margin_top(px(TOP * zoom))
            .close_button(false)
            .child(palette.clone())
    });
    window.defer(cx, move |window, cx| {
        state.update(cx, |state, cx| state.focus(window, cx));
    });
}

/// The palette in a GPUI test window: typed into and confirmed through gpui-kit's
/// own key bindings, as the user does.
#[cfg(test)]
mod view_tests {
    use std::cell::RefCell;

    use gitweft_ui::SettingsStore;
    use gpui_kit::component::Root;
    // Rust's own `#[test]`: `gpui_kit::*` (through `super::*`) brings GPUI's `test` macro.
    use std::prelude::v1::test;

    use super::*;

    /// An empty window's view. Like the app's `Shell`, it draws the kit's dialog layer:
    /// `Root` keeps the open dialogs, but the view under it has to draw them.
    struct Blank;

    impl Render for Blank {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .children(Root::render_dialog_layer(window, cx))
        }
    }

    /// A window (a `Root` around a blank view, as the app's) with the palette open on three
    /// commands that record their label when run; returns the record.
    fn palette(cx: &mut TestAppContext, dir: &std::path::Path) -> Rc<RefCell<Vec<&'static str>>> {
        cx.update(|cx| {
            gpui_kit::init(cx);
            SettingsStore::init_with_file(cx, &dir.join("settings.json"));
        });
        let ran = Rc::new(RefCell::new(Vec::new()));
        let commands = ["Fetch all remotes", "Rebase the branch", "Show the reflog"]
            .into_iter()
            .map(|label| {
                let ran = ran.clone();
                PaletteCommand::new("Repository", label, move |_, _| {
                    ran.borrow_mut().push(label)
                })
            })
            .collect();
        let (_, window) = cx.add_window_view(|window, cx| {
            let blank = cx.new(|_| Blank);
            Root::new(blank, window, cx)
        });
        window.update(|window, cx| open(commands, window, cx));
        window.run_until_parked();
        assert!(window.update(|window, cx| window.has_active_dialog(cx)));
        frame(cx);
        ran
    }

    /// Draws the window's frame. The test platform draws only when asked, and keys go by
    /// the last frame drawn: the dialog, and the query field it focuses just after opening,
    /// must be in one before keys can reach them.
    fn frame(cx: &mut TestAppContext) {
        let window = cx.windows()[0];
        cx.update_window(window, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        cx.run_until_parked();
    }

    fn dialog_open(cx: &mut TestAppContext) -> bool {
        let window = cx.windows()[0];
        cx.update_window(window, |_, window, cx| window.has_active_dialog(cx))
            .unwrap()
    }

    #[test]
    fn a_typed_query_and_enter_run_the_best_match_and_close_the_palette() {
        let dir = tempfile::tempdir().unwrap();
        let mut cx = TestAppContext::single();
        let ran = palette(&mut cx, dir.path());
        let window = cx.windows()[0];
        cx.simulate_input(window, "reb");
        frame(&mut cx);
        cx.simulate_keystrokes(window, "enter");
        cx.run_until_parked();
        assert_eq!(*ran.borrow(), ["Rebase the branch"]);
        assert!(!dialog_open(&mut cx));
    }

    #[test]
    fn escape_closes_the_palette_and_runs_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut cx = TestAppContext::single();
        let ran = palette(&mut cx, dir.path());
        let window = cx.windows()[0];
        cx.simulate_keystrokes(window, "escape");
        cx.run_until_parked();
        assert!(ran.borrow().is_empty());
        assert!(!dialog_open(&mut cx));
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_SHOWN, Match, fuzzy, rank, sections};
    use gpui_kit::SharedString;

    fn term(text: &str) -> Vec<char> {
        text.chars().collect()
    }

    #[test]
    fn words_all_in_the_label_come_before_a_detail_match() {
        // The diff's file Stage (detail: the file) and the selected lines' Stage.
        let commands = [
            ("Stage", Some("lines.txt")),
            ("Stage hunk", Some("lines.txt @@ -1,6 +1,6 @@")),
            ("Stage lines", Some("2 lines selected")),
        ];
        let found = rank("stage lines", commands);
        assert_eq!(found.first().map(|(ix, _)| *ix), Some(2));
        // A word only the detail has still finds the command.
        let found = rank("stage txt", commands);
        assert_eq!(found.first().map(|(ix, _)| *ix), Some(0));
    }

    #[test]
    fn a_long_list_is_cut_and_says_how_much() {
        let (rebase, window): (SharedString, SharedString) = ("Rebase".into(), "Window".into());
        // A huge first group (a rebase's rows), then the window's three commands.
        let group = |ix: usize| {
            if ix < MAX_SHOWN + 50 {
                &rebase
            } else {
                &window
            }
        };
        let ranked: Vec<(usize, Match)> = (0..MAX_SHOWN + 53)
            .map(|ix| (ix, Match::default()))
            .collect();
        // Nothing typed: each group is cut on its own, so the window's commands stay.
        let (shown, hidden) = sections(true, ranked.clone(), group);
        assert_eq!(hidden, 50);
        assert_eq!(shown.len(), 2);
        assert_eq!(shown[0].1.len(), MAX_SHOWN);
        assert_eq!(shown[1].0.as_ref(), Some(&window));
        assert_eq!(shown[1].1.len(), 3);
        // Typed: the best matches only, in one list.
        let (shown, hidden) = sections(false, ranked, group);
        assert_eq!(hidden, 53);
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].1.len(), MAX_SHOWN);
        // A short list is whole.
        let few: Vec<(usize, Match)> = (0..3).map(|ix| (ix, Match::default())).collect();
        assert_eq!(sections(true, few, |_| &window).1, 0);
    }

    fn offsets(text: &str, word: &str) -> Vec<usize> {
        fuzzy(&term(word), text).expect("matches").1
    }

    #[test]
    fn a_general_command_wins_a_tie_over_the_same_action_on_a_row() {
        // "reflog" matches both labels at a word start: HEAD's reflog (no detail) comes
        // before each branch's "Show reflog" (the branch as its detail), shorter or not.
        let commands = [
            ("Show reflog", Some("feature/review")),
            ("Show reflog", Some("main")),
            ("Show the reflog of HEAD", None),
        ];
        let found: Vec<usize> = rank("reflog", commands)
            .into_iter()
            .map(|(ix, _)| ix)
            .collect();
        assert_eq!(found, [2, 0, 1]);
        // A better match still wins over the tie-break.
        let found: Vec<usize> = rank("main reflog", commands)
            .into_iter()
            .map(|(ix, _)| ix)
            .collect();
        assert_eq!(found, [1]);
    }

    #[test]
    fn a_term_matches_in_order_ignoring_case() {
        assert!(fuzzy(&term("fch"), "Fetch").is_some());
        assert!(fuzzy(&term("hcf"), "Fetch").is_none());
        assert!(fuzzy(&term("fetchx"), "Fetch").is_none());
        assert_eq!(fuzzy(&term(""), "Fetch"), Some((0, Vec::new())));
    }

    #[test]
    fn word_starts_and_runs_are_preferred() {
        assert_eq!(offsets("Force push (with lease)", "pu"), vec![6, 7]);
        // The initials of words: the "h" of "here", not the one in "branch".
        assert_eq!(offsets("New branch here", "nbh"), vec![0, 4, 11]);
        // A run beats letters spread out.
        let (run, _) = fuzzy(&term("pul"), "Pull").unwrap();
        let (spread, _) = fuzzy(&term("pul"), "Pick up all").unwrap();
        assert!(run > spread);
    }

    #[test]
    fn letters_scattered_inside_words_are_no_match() {
        let path = "/tmp/user-1000/-home-user-Development-Projects-Tools-Weft/g2/app";
        assert!(fuzzy(&term("stash"), path).is_none());
        assert!(fuzzy(&term("lib"), path).is_none());
        // Skipping inside a word is fine once: "nxt" is still "Next".
        assert!(fuzzy(&term("nxt"), "Next tab").is_some());
        assert!(fuzzy(&term("fch"), "Fetch").is_some());
    }

    #[test]
    fn offsets_are_bytes() {
        assert_eq!(offsets("Öffne Ärger", "är"), vec![7, 9]);
    }

    #[test]
    fn every_word_must_match_label_or_detail_in_any_order() {
        let commands = [
            ("Merge into current", Some("feature/login")),
            ("Merge into current", Some("main")),
            ("Delete branch…", Some("feature/login")),
        ];
        let found = rank("login merge", commands);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, 0);
        assert_eq!(found[0].1.label, vec![0, 1, 2, 3, 4]);
        assert_eq!(found[0].1.detail, vec![8, 9, 10, 11, 12]);
        assert!(rank("rebase", commands).is_empty());
    }

    #[test]
    fn the_best_match_comes_first_and_a_blank_query_keeps_everything() {
        let commands = [
            ("Pull (rebase)", None),
            ("Push", None),
            ("Apply patches…", None),
        ];
        let found: Vec<usize> = rank("pu", commands).into_iter().map(|(ix, _)| ix).collect();
        // Both start with "Pu": the shorter label wins the tie. "Apply patches" has no "u".
        assert_eq!(found, vec![1, 0]);
        let all: Vec<usize> = rank("  ", commands).into_iter().map(|(ix, _)| ix).collect();
        assert_eq!(all, vec![0, 1, 2]);
    }
}
