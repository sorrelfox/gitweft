//! Headless UI tests: the app's own window (a `Root` around the shell, every key binding
//! the app registers) on sandbox repositories, driven by keys and typed text, checked
//! against git and the views' state. They take the place of desktop click-tests for what
//! doesn't need a real desktop: no screenshots, no coordinates, run by `cargo test`.
//!
//! Git work runs on worker threads, which `run_until_parked` doesn't wait for: [`Ui::wait`]
//! draws and polls until a condition holds.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use gitweft_testkit::Sandbox;
use gitweft_ui::SettingsStore;
use gpui_kit::component::Root;
use gpui_kit::*;

use crate::shell::Shell;
use crate::workspace::{Workspace, WorkspaceEnv};

/// The sequence editor git runs during a rebase: the app's binary handles
/// `--sequence-editor PLAN TODO`, but a test binary doesn't. This stand-in writes the plan's
/// todo list for git (without the app's check that git listed the planned commits).
pub(crate) static SEQUENCE_EDITOR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

fn install_sequence_editor() {
    SEQUENCE_EDITOR.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("gitweft-ui-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("sequence-editor");
        // `$1` is `--sequence-editor`, `$2` the plan, `$3` git's todo file.
        std::fs::write(&script, "#!/bin/sh\nsed '1,/^todo$/d' \"$2\" > \"$3\"\n").unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    });
}

/// How long [`Ui::wait`] gives git before failing the test.
const PATIENCE: Duration = Duration::from_secs(10);

pub(crate) struct Ui<'a> {
    pub window: &'a mut VisualTestContext,
    pub shell: Entity<Shell>,
}

/// A window with a tab per repository, the first active, drawn once. Settings and
/// per-repository data live in the sandbox, never the user's.
pub(crate) fn open<'a>(cx: &'a mut TestAppContext, sb: &Sandbox, repos: Vec<PathBuf>) -> Ui<'a> {
    let dir = sb.path().join("data");
    std::fs::create_dir_all(&dir).unwrap();
    install_sequence_editor();
    cx.update(|cx| {
        gpui_kit::init(cx);
        SettingsStore::init_with_file(cx, &sb.path().join("settings.json"));
        crate::bind_keys(cx);
    });
    let mut shell = None;
    let (_, window) = cx.add_window_view(|window, cx| {
        let env = WorkspaceEnv::isolated(&dir);
        let view = cx.new(|cx| Shell::new(repos, 0, None, env, window, cx));
        shell = Some(view.clone());
        Root::new(view, window, cx)
    });
    let mut ui = Ui {
        window,
        shell: shell.expect("the shell was built"),
    };
    ui.frame();
    ui
}

impl Ui<'_> {
    /// Runs what's ready and draws a frame: keys go by the last frame drawn.
    pub fn frame(&mut self) {
        self.window.run_until_parked();
        self.window.update(|window, cx| window.draw(cx).clear(cx));
        self.window.run_until_parked();
    }

    /// Draws until `done` holds, for up to [`PATIENCE`]; fails the test naming `what`.
    pub fn wait(&mut self, what: &str, mut done: impl FnMut(&mut Self) -> bool) {
        let start = Instant::now();
        loop {
            self.frame();
            if done(self) {
                return;
            }
            if start.elapsed() >= PATIENCE {
                // What git did (reads left out) says most about why it didn't happen.
                let ran: Vec<String> = self
                    .read(|w, _| w.activity())
                    .into_iter()
                    .filter(|entry| !entry.read_only)
                    .map(|entry| format!("  {} -> {:?}", entry.command, entry.outcome))
                    .collect();
                panic!(
                    "timed out waiting for: {what}\ngit commands run:\n{}",
                    ran.join("\n")
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Presses keys (`"ctrl-a space"`), then draws.
    pub fn keys(&mut self, keys: &str) {
        self.window.simulate_keystrokes(keys);
        self.frame();
    }

    /// Types text into the focused field, then draws.
    pub fn type_text(&mut self, text: &str) {
        self.window.simulate_input(text);
        self.frame();
    }

    /// The active tab's workspace.
    pub fn workspace(&mut self) -> Entity<Workspace> {
        let shell = self.shell.clone();
        self.window.update(|_, cx| shell.read(cx).workspace(0))
    }

    /// Reads from the active tab's workspace.
    pub fn read<T>(&mut self, f: impl FnOnce(&Workspace, &App) -> T) -> T {
        let workspace = self.workspace();
        self.window.update(|_, cx| f(workspace.read(cx), cx))
    }

    /// Clicks the graph's row `row` (0 is the top), which also gives the graph the keys.
    pub fn click_row(&mut self, row: usize) {
        self.click_row_with(row, Modifiers::none(), 1);
    }

    /// Clicks the graph's row `row` with `modifiers` held; `clicks` 2 is a double click.
    pub fn click_row_with(&mut self, row: usize, modifiers: Modifiers, clicks: usize) {
        // `debug_bounds` takes a `&'static str`.
        let name: &'static str = Box::leak(format!("graph-row-{row}").into_boxed_str());
        let position = self
            .window
            .debug_bounds(name)
            .unwrap_or_else(|| panic!("graph row {row} isn't drawn"))
            .center();
        for click_count in 1..=clicks {
            let button = MouseButton::Left;
            self.window.simulate_event(MouseDownEvent {
                position,
                modifiers,
                button,
                click_count,
                first_mouse: false,
            });
            self.window.simulate_event(MouseUpEvent {
                position,
                modifiers,
                button,
                click_count,
            });
            self.frame();
        }
    }

    /// The graph's row showing `rev` of `repo`.
    pub fn row_of(&mut self, sb: &Sandbox, repo: &std::path::Path, rev: &str) -> usize {
        let id = commit_id(sb, repo, rev);
        self.read(|w, cx| w.graph().and_then(|graph| graph.read(cx).row_of(id)))
            .unwrap_or_else(|| panic!("{rev} isn't in the graph"))
    }

    /// The commit selected in the graph.
    pub fn selected(&mut self) -> Option<gitweft_git::ObjectId> {
        self.read(|w, cx| w.graph().and_then(|graph| graph.read(cx).selected_id()))
    }

    /// Whether a dialog (ours, in the window) is open.
    pub fn dialog_open(&mut self) -> bool {
        use gpui_kit::component::WindowExt as _;
        self.window
            .update(|window, cx| window.has_active_dialog(cx))
    }

    /// Waits for a confirmation prompt and presses its button `button`.
    pub fn answer_prompt(&mut self, button: &str) {
        self.wait("a prompt", |ui| ui.window.has_pending_prompt());
        self.window.simulate_prompt_answer(button);
        self.frame();
    }

    /// Runs a command through the palette: Ctrl+Shift+P, the query, Enter.
    pub fn palette(&mut self, query: &str) {
        self.keys("ctrl-shift-p");
        self.type_text(query);
        self.keys("enter");
    }
}

/// The id of `rev` in `repo`.
pub(crate) fn commit_id(sb: &Sandbox, repo: &std::path::Path, rev: &str) -> gitweft_git::ObjectId {
    let hex = sb.run(repo, &["rev-parse", "--verify", rev]);
    gitweft_git::ObjectId::from_hex(hex.trim().as_bytes()).expect("git gives a hex id")
}

/// `git status --porcelain` of `repo`, one entry per line. Without the index lock that
/// status takes when it can: the tests poll it while the app writes, which then fails on
/// that lock, as it would beside a user's own `git status`.
pub(crate) fn status(sb: &Sandbox, repo: &std::path::Path) -> String {
    sb.run(repo, &["--no-optional-locks", "status", "--porcelain"])
}

/// The subjects of `repo`'s commits on HEAD, newest first, joined by `|`.
pub(crate) fn subjects(sb: &Sandbox, repo: &std::path::Path) -> String {
    sb.run(repo, &["log", "--format=%s"])
        .trim_end()
        .replace('\n', "|")
}

/// A repository with commits c1…c4, each adding its own file fN.txt. Its own config names
/// the committer: the app's git reads no global config in a test.
pub(crate) fn four_commits(sb: &Sandbox) -> PathBuf {
    let repo = sb.init_repo("r");
    sb.run(&repo, &["config", "user.name", "Tester"]);
    sb.run(&repo, &["config", "user.email", "tester@example.com"]);
    for n in 1..=4 {
        sb.commit_file(&repo, &format!("f{n}.txt"), "x\n", &format!("c{n}"));
    }
    repo
}

/// The conflict tool's fixture (was graphite-conflict-fixture.sh): a merge of feat into main
/// stopped on two conflicts in timeline.rs, at line 5 and at line 15 (where main has two
/// lines, `line_15` and `line_15b`), and one in notes.txt.
pub(crate) fn conflicted_merge(sb: &Sandbox) -> PathBuf {
    let repo = sb.init_repo("r");
    sb.run(&repo, &["config", "user.name", "Tester"]);
    sb.run(&repo, &["config", "user.email", "tester@example.com"]);
    let lines = |edit: &dyn Fn(usize) -> Option<String>| -> String {
        (1..=20)
            .map(|n| edit(n).unwrap_or_else(|| format!("let line_{n} = 0;\n")))
            .collect()
    };
    std::fs::write(repo.join("timeline.rs"), lines(&|_| None)).unwrap();
    std::fs::write(repo.join("notes.txt"), "base\n").unwrap();
    sb.run(&repo, &["add", "."]);
    sb.run(&repo, &["commit", "-qm", "base"]);
    sb.run(&repo, &["switch", "-qc", "feat"]);
    let theirs = lines(&|n| match n {
        5 => Some("let line_5 = stack.edited();\n".into()),
        15 => Some("let line_15 = theirs();\n".into()),
        _ => None,
    });
    std::fs::write(repo.join("timeline.rs"), theirs).unwrap();
    std::fs::write(repo.join("notes.txt"), "theirs\n").unwrap();
    sb.run(&repo, &["commit", "-qam", "feat edit"]);
    sb.run(&repo, &["switch", "-q", "main"]);
    let ours = lines(&|n| match n {
        5 => Some("let line_5 = stack.assets[0];\n".into()),
        15 => Some("let line_15 = ours();\nlet line_15b = ours_extra();\n".into()),
        _ => None,
    });
    std::fs::write(repo.join("timeline.rs"), ours).unwrap();
    std::fs::write(repo.join("notes.txt"), "ours\n").unwrap();
    sb.run(&repo, &["commit", "-qam", "main edit"]);
    // The merge stops on the conflicts: git exits 1, which `run` would take for a failure.
    let merged = sb
        .git()
        .command(&repo)
        .args(["merge", "-q", "feat"])
        .run(&gitweft_git::CancelToken::new());
    assert!(merged.is_err(), "the merge conflicts");
    repo
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_i18n::t;
    // Rust's own `#[test]`: `gpui_kit::*` brings GPUI's `test` macro.
    use std::prelude::v1::test;

    /// A window on `repo` whose status has been read, the working changes shown and the
    /// file lists holding the keys.
    fn staging_keys<'a>(
        cx: &'a mut TestAppContext,
        sb: &Sandbox,
        repo: &std::path::Path,
        changed: usize,
    ) -> Ui<'a> {
        let mut ui = open(cx, sb, vec![repo.to_owned()]);
        ui.wait("the status is read", |ui| {
            ui.read(|w, cx| {
                let status = w.staging().read(cx).status();
                status.unstaged().count() + status.staged().count() == changed
            })
        });
        // The palette offers the command once the workspace has read the status too, which
        // may be later than the staging view: run it before, and Enter takes another one.
        ui.wait("the palette offers the working changes", |ui| {
            labels(ui).iter().any(|l| l == "Show working changes")
        });
        ui.palette("show working changes");
        // Keys given to lists that aren't drawn go nowhere.
        ui.wait("the file lists are drawn", |ui| {
            ui.window.debug_bounds("staging-unstaged-list").is_some()
                || ui.window.debug_bounds("staging-staged-list").is_some()
        });
        let focus = ui.read(|w, cx| w.staging().read(cx).file_lists_focus());
        ui.window.update(|window, cx| focus.focus(window, cx));
        ui.wait("the file lists have the keys", |ui| {
            ui.window.update(|window, _| focus.is_focused(window))
        });
        ui
    }

    /// The cursor walks the unstaged list into the staged one: End reaches the staged
    /// file, and Space there unstages it in git; the other file isn't touched.
    #[test]
    fn space_on_a_staged_file_unstages_it_alone() {
        let sb = Sandbox::new();
        let repo = sb.init_repo("r");
        sb.commit_file(&repo, "a.txt", "one\n", "c1");
        sb.commit_file(&repo, "b.txt", "one\n", "c2");
        std::fs::write(repo.join("a.txt"), "staged\n").unwrap();
        std::fs::write(repo.join("b.txt"), "not staged\n").unwrap();
        sb.run(&repo, &["add", "a.txt"]);
        let mut cx = TestAppContext::single();
        let mut ui = staging_keys(&mut cx, &sb, &repo, 2);

        ui.keys("end space");

        ui.wait("git unstaged a.txt", |_| {
            status(&sb, &repo) == " M a.txt\n M b.txt\n"
        });
    }

    /// The working changes shown from the palette, the file lists given the keys (as F6
    /// does), Ctrl+A and Space: git stages every file, and the lists follow.
    /// (Was graphite-g15c-test.sh's step 4 and g12c's multi-select, on the desktop.)
    #[test]
    fn ctrl_a_and_space_stage_every_changed_file_in_git() {
        let sb = Sandbox::new();
        let repo = sb.init_repo("r");
        sb.commit_file(&repo, "b.txt", "one\n", "c1");
        std::fs::write(repo.join("b.txt"), "changed\n").unwrap();
        for n in 1..=3 {
            std::fs::write(repo.join(format!("file{n}.txt")), "new\n").unwrap();
        }
        let mut cx = TestAppContext::single();
        let mut ui = staging_keys(&mut cx, &sb, &repo, 4);

        ui.keys("ctrl-a space");

        ui.wait("git staged every file", |_| {
            status(&sb, &repo)
                .lines()
                .all(|line| !line.starts_with("??") && !line.starts_with(" "))
        });
        assert_eq!(
            status(&sb, &repo),
            "M  b.txt\nA  file1.txt\nA  file2.txt\nA  file3.txt\n"
        );
        ui.wait("the lists follow", |ui| {
            ui.read(|w, cx| w.staging().read(cx).status().unstaged().count() == 0)
        });
    }

    /// The palette's labels as they stand in the active tab.
    fn labels(ui: &mut Ui) -> Vec<String> {
        let workspace = ui.workspace();
        ui.window
            .update(|_, cx| workspace.update(cx, |w, cx| w.palette_labels(cx)))
    }

    /// A window on `repo` with its graph drawn.
    fn graph<'a>(cx: &'a mut TestAppContext, sb: &Sandbox, repo: &std::path::Path) -> Ui<'a> {
        let mut ui = open(cx, sb, vec![repo.to_owned()]);
        ui.wait("the graph is drawn", |ui| {
            ui.window.debug_bounds("graph-row-3").is_some()
        });
        ui
    }

    /// A row's Reword… from the palette: the dialog holds the message; replaced and
    /// confirmed, git has the new message and the commits after it kept.
    /// (Was graphite-quick-rebase-test.sh step 1 and g15g's reword, on the desktop.)
    #[test]
    fn reword_from_the_palette_rewrites_the_selected_commit() {
        let sb = Sandbox::new();
        let repo = four_commits(&sb);
        let mut cx = TestAppContext::single();
        let mut ui = graph(&mut cx, &sb, &repo);

        ui.click_row(1); // c3
        ui.palette("reword");
        ui.wait("the message dialog", |ui| ui.dialog_open());
        ui.keys("ctrl-a");
        ui.type_text("c3, reworded");
        ui.keys("enter");

        ui.wait("git has the new message", |_| {
            subjects(&sb, &repo) == "c4|c3, reworded|c2|c1"
        });
    }

    /// Squash into parent: the commit's change joins its parent's, one commit fewer.
    #[test]
    fn squash_into_parent_folds_the_commit_into_the_one_before() {
        let sb = Sandbox::new();
        let repo = four_commits(&sb);
        let mut cx = TestAppContext::single();
        let mut ui = graph(&mut cx, &sb, &repo);

        ui.click_row(1); // c3
        ui.palette("squash into parent");

        ui.wait("c3 folded into c2", |_| subjects(&sb, &repo) == "c4|c2|c1");
        let files = sb.run(&repo, &["show", "--name-only", "--format=", "HEAD~1"]);
        assert_eq!(files, "f2.txt\nf3.txt\n");
        // A squash keeps the folded commit's message too (a fixup wouldn't).
        let message = sb.run(&repo, &["log", "-1", "--format=%B", "HEAD~1"]);
        assert!(message.contains("c3"), "{message:?}");
    }

    /// Drop commit asks first; Drop removes the commit and keeps the ones after it.
    #[test]
    fn drop_commit_asks_then_removes_the_commit() {
        let sb = Sandbox::new();
        let repo = four_commits(&sb);
        let mut cx = TestAppContext::single();
        let mut ui = graph(&mut cx, &sb, &repo);

        ui.click_row(2); // c2
        ui.palette("drop commit");
        ui.answer_prompt(&t!("rebase.drop_button"));

        ui.wait("c2 dropped", |_| subjects(&sb, &repo) == "c4|c3|c1");
        assert!(!repo.join("f2.txt").exists());
    }

    /// Our prompt holds the window while it's open: a click on the graph behind it selects
    /// nothing (before, a click or a right-click there acted, menus included). Escape
    /// answers Cancel, and then the graph takes clicks again.
    #[test]
    fn the_window_behind_a_prompt_takes_no_clicks() {
        let sb = Sandbox::new();
        let repo = four_commits(&sb);
        let mut cx = TestAppContext::single();
        cx.update(|cx| cx.set_prompt_builder(crate::prompt::build));
        let mut ui = graph(&mut cx, &sb, &repo);
        let c2 = commit_id(&sb, &repo, "HEAD~2");
        let c4 = commit_id(&sb, &repo, "HEAD");

        ui.click_row(2);
        ui.palette("drop commit");
        ui.wait("the prompt", |ui| {
            ui.window.debug_bounds("prompt").is_some()
        });
        ui.click_row(0);
        assert_eq!(
            ui.selected(),
            Some(c2),
            "a click behind the prompt selected a row"
        );

        ui.keys("escape");
        ui.wait("the prompt closed", |ui| {
            ui.window.debug_bounds("prompt").is_none()
        });
        assert_eq!(subjects(&sb, &repo), "c4|c3|c2|c1", "Cancel drops nothing");
        ui.click_row(0);
        assert_eq!(ui.selected(), Some(c4));
    }

    /// Prompts, dialogs and toasts (`gitweft_ui::flow`) break their lines at spaces only:
    /// "(Ctrl+Z)" goes down whole where GPUI split it before "+". A blank line keeps
    /// its height.
    #[test]
    fn flow_breaks_at_spaces_and_keeps_blank_lines() {
        struct Flow(Vec<&'static str>);
        impl Render for Flow {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                gpui_kit::component::v_flex()
                    .w(px(120.))
                    .children(self.0.iter().enumerate().map(|(ix, text)| {
                        let name: &'static str = Box::leak(format!("flow-{ix}").into_boxed_str());
                        div()
                            .debug_selector(move || name.into())
                            .child(gitweft_ui::flow::flow(ix, *text))
                    }))
            }
        }
        let mut cx = TestAppContext::single();
        cx.update(gpui_kit::init);
        let texts = vec!["one", "one\ntwo", "one\n\ntwo", "Undo it (Ctrl+Z)"];
        let (_, window) = cx.add_window_view(|_, _| Flow(texts));
        for _ in 0..3 {
            window.run_until_parked();
            window.update(|window, cx| window.draw(cx).clear(cx));
        }
        let mut height = |name: &'static str| window.debug_bounds(name).unwrap().size.height;
        let line = height("flow-0");
        assert!(line > px(0.));
        assert_eq!(height("flow-1"), line * 2., "a line per row");
        assert_eq!(
            height("flow-2"),
            line * 3.,
            "the blank line keeps its height"
        );
        assert_eq!(height("flow-3"), line * 2.);
        assert!(
            window.debug_bounds("flow:Undo it\n(Ctrl+Z)").is_some(),
            "the line broke at the space before (Ctrl+Z)"
        );
    }

    /// The pointer leaving the window ends the hover: GPUI kept its last position, so the
    /// button under it kept its hover look and tooltip. A leave mid-drag keeps it.
    #[test]
    fn the_pointer_leaving_the_window_ends_the_hover() {
        let sb = Sandbox::new();
        let repo = four_commits(&sb);
        let mut cx = TestAppContext::single();
        let mut ui = graph(&mut cx, &sb, &repo);
        let row = ui.window.debug_bounds("graph-row-0").unwrap().center();
        let modifiers = Modifiers::none();
        let leave = |ui: &mut Ui, pressed_button| {
            ui.window.simulate_event(MouseExitEvent {
                position: row,
                pressed_button,
                modifiers,
            });
            ui.frame();
            ui.window.update(|window, _| window.mouse_position())
        };

        ui.window.simulate_mouse_move(row, None, modifiers);
        ui.frame();
        assert_eq!(
            leave(&mut ui, Some(MouseButton::Left)),
            row,
            "a leave during a drag keeps the pointer where it was"
        );
        let after = leave(&mut ui, None);
        assert!(
            after.x < px(0.) && after.y < px(0.),
            "the pointer stayed at {after:?}, in the window"
        );
    }

    /// Create fixup! commit: what's staged becomes `fixup! <summary>` on top.
    #[test]
    fn create_fixup_commits_the_staged_change_for_the_selected_commit() {
        let sb = Sandbox::new();
        let repo = four_commits(&sb);
        std::fs::write(repo.join("f2.txt"), "fixed\n").unwrap();
        sb.run(&repo, &["add", "f2.txt"]);
        let mut cx = TestAppContext::single();
        let mut ui = graph(&mut cx, &sb, &repo);
        ui.wait("the staged change is read", |ui| {
            ui.read(|w, cx| w.staging().read(cx).status().staged().count() == 1)
        });

        ui.click_row(2); // c2 (the working changes row isn't counted)
        ui.palette("create fixup");

        ui.wait("the fixup! commit", |_| {
            subjects(&sb, &repo) == "fixup! c2|c4|c3|c2|c1"
        });
        assert_eq!(status(&sb, &repo), "");
    }

    /// The conflict tool by keyboard and palette: Next conflict, one line of ours and one of
    /// theirs picked with the arrows and Space (ours first in the result), Previous
    /// conflict, Take theirs, then Save & mark resolved: git has the lines picked, and
    /// timeline.rs staged. (Was graphite-pick-lines-test.sh and g15h2's conflict part.)
    #[test]
    fn picked_lines_and_a_taken_side_are_saved_and_staged() {
        let sb = Sandbox::new();
        let repo = conflicted_merge(&sb);
        let mut cx = TestAppContext::single();
        let mut ui = open(&mut cx, &sb, vec![repo.clone()]);
        // notes.txt is row 0, timeline.rs row 1.
        ui.wait("the conflicted files panel", |ui| {
            ui.window.debug_bounds("conflict-file-1").is_some()
        });
        let at = ui.window.debug_bounds("conflict-file-1").unwrap().center();
        ui.window.simulate_click(at, Modifiers::none());
        ui.wait("the conflict tool", |ui| {
            labels(ui).iter().any(|label| label == "Next conflict")
        });

        ui.palette("next conflict");
        ui.keys("down down space"); // ours: line_15b
        ui.keys("right space"); // theirs: line_15
        ui.palette("previous conflict");
        ui.palette("take theirs");
        ui.palette("save & mark resolved");

        ui.wait("git has timeline.rs resolved", |_| {
            status(&sb, &repo) == "UU notes.txt\nM  timeline.rs\n"
        });
        let text = std::fs::read_to_string(repo.join("timeline.rs")).unwrap();
        let picked: Vec<&str> = text
            .lines()
            .filter(|line| line.starts_with("let line_5 ") || line.starts_with("let line_15"))
            .collect();
        assert_eq!(
            picked,
            [
                "let line_5 = stack.edited();",
                "let line_15b = ours_extra();",
                "let line_15 = theirs();"
            ]
        );
    }

    /// The rebase editor from a row, through the palette: a pause after c2 and c4 squashed
    /// into c3, Start: git stops after c2; Continue finishes with c4 folded; Undo takes the
    /// whole rebase back. (Was graphite-rebase-test.sh and g15h2's editor part.)
    #[test]
    fn a_planned_rebase_pauses_continues_and_is_undone_whole() {
        let sb = Sandbox::new();
        let repo = four_commits(&sb);
        let before = sb.run(&repo, &["rev-parse", "HEAD"]);
        let mut cx = TestAppContext::single();
        let mut ui = graph(&mut cx, &sb, &repo);
        let rebasing = || repo.join(".git/rebase-merge").exists();

        ui.click_row(3); // c1
        ui.palette("interactive rebase after this commit");
        ui.wait("the rebase editor", |ui| {
            labels(ui).iter().any(|l| l == "Pause after this commit")
        });
        ui.palette("pause after c2");
        ui.palette("squash c4");
        ui.palette("start rebase");

        ui.wait("git paused after c2", |_| {
            rebasing() && sb.run(&repo, &["log", "-1", "--format=%s"]) == "c2\n"
        });
        // Offered once the app has seen the pause (git stopping is earlier).
        ui.wait("the rebase can be continued", |ui| {
            labels(ui).iter().any(|l| l == "Continue the rebase")
        });
        ui.palette("continue the rebase");
        ui.wait("the rebase finished", |_| {
            !rebasing() && subjects(&sb, &repo) == "c3|c2|c1"
        });

        // Undo offers the rebase once the app has recorded it (git finishing is earlier).
        ui.wait("the rebase can be undone", |ui| {
            labels(ui)
                .iter()
                .any(|l| l.starts_with("Undo") && l.contains("rebase"))
        });
        ui.palette("undo");
        ui.wait("the whole rebase undone", |_| {
            sb.run(&repo, &["rev-parse", "HEAD"]) == before
        });
        assert_eq!(subjects(&sb, &repo), "c4|c3|c2|c1");
    }

    /// Tab to a link below the details' fold scrolls the panel to it. The
    /// checked-out commit's long body pushes the facts under the fold; from the file list,
    /// Shift+Tab twice ("Show more") and Tab reach the parent's SHA, which must then lie
    /// inside the visible part. (Was graphite-reveal-focus-test.sh.)
    #[test]
    fn tab_to_the_parent_below_the_fold_scrolls_it_into_view() {
        let sb = Sandbox::new();
        let repo = sb.init_repo("r");
        sb.commit_file(&repo, "a.txt", "one\n", "An earlier commit");
        let body: String = (1..=40)
            .map(|n| format!("Body line {n}, long enough to fill the details panel.\n"))
            .collect();
        std::fs::write(repo.join("a.txt"), "two\n").unwrap();
        sb.run(&repo, &["add", "a.txt"]);
        let message = format!("A commit with a long body\n\n{body}");
        sb.run(&repo, &["commit", "-qm", &message]);
        let mut cx = TestAppContext::single();
        let mut ui = open(&mut cx, &sb, vec![repo.clone()]);
        ui.wait("the details with the parent", |ui| {
            ui.window.debug_bounds("details-parent-0").is_some()
        });
        // The whole body ("Show more"): taller than the details' 60 %, so the parent
        // is below the fold. Clicked once the toggle is wholly in view: until the panel
        // has been measured, the details' cap is a guess that can leave the toggle half
        // under the fold, where the click would miss it.
        let toggle = |ui: &mut Ui| {
            let top = ui.window.debug_bounds("details-top")?;
            let toggle = ui.window.debug_bounds("details-body-toggle")?;
            (toggle.bottom() <= top.bottom()).then(|| toggle.center())
        };
        ui.wait("the toggle in view", |ui| toggle(ui).is_some());
        let at = toggle(&mut ui).unwrap();
        ui.window.simulate_click(at, Modifiers::none());
        ui.frame();
        let top = ui.window.debug_bounds("details-top").unwrap();
        let parent = ui.window.debug_bounds("details-parent-0").unwrap();
        assert!(
            parent.top() >= top.bottom(),
            "the fixture puts the parent below the fold"
        );

        let focus = ui.read(|w, cx| w.details().read(cx).focus_handle(cx));
        ui.window.update(|window, cx| focus.focus(window, cx));
        ui.frame();
        ui.keys("shift-tab shift-tab tab");
        ui.frame();

        let top = ui.window.debug_bounds("details-top").unwrap();
        let parent = ui.window.debug_bounds("details-parent-0").unwrap();
        assert!(
            parent.top() >= top.top() && parent.bottom() <= top.bottom(),
            "the focused parent {parent:?} lies inside the details' visible part {top:?}"
        );
    }

    /// The repository's own settings file (hidden refs, solo, walk options), as saved.
    fn repo_settings(repo: &std::path::Path) -> String {
        let file = gitweft_core::repo_settings::settings_file(repo);
        let text = std::fs::read_to_string(file).unwrap_or_default();
        text.split_whitespace().collect()
    }

    /// How many commit rows the graph draws (up to `max`).
    fn rows(ui: &mut Ui, max: usize) -> usize {
        (0..max)
            .take_while(|row| {
                let name: &'static str = Box::leak(format!("graph-row-{row}").into_boxed_str());
                ui.window.debug_bounds(name).is_some()
            })
            .count()
    }

    /// A branch hidden from the palette leaves the graph and is saved for the repository;
    /// "Show every branch" brings it back; soloed, only its history shows.
    /// (Was graphite-hide-solo-test.sh.)
    #[test]
    fn hidden_and_solo_branches_change_the_graph_and_are_saved() {
        let sb = Sandbox::new();
        let repo = sb.init_repo("r");
        sb.run(&repo, &["config", "user.name", "Tester"]);
        sb.run(&repo, &["config", "user.email", "tester@example.com"]);
        sb.commit_file(&repo, "a.txt", "1\n", "c1");
        sb.run(&repo, &["switch", "-qc", "topic"]);
        sb.commit_file(&repo, "t.txt", "t\n", "t1");
        sb.run(&repo, &["switch", "-q", "main"]);
        sb.commit_file(&repo, "b.txt", "2\n", "c2");
        let mut cx = TestAppContext::single();
        let mut ui = open(&mut cx, &sb, vec![repo.clone()]);
        ui.wait("three commits", |ui| rows(ui, 5) == 3);

        ui.palette("hide in graph topic");
        // The settings are saved in the background, maybe after the graph has changed.
        ui.wait(
            "topic's commit leaves, and the hidden branch is saved",
            |ui| rows(ui, 5) == 2 && repo_settings(&repo).contains("refs/heads/topic"),
        );

        ui.palette("show every branch");
        ui.wait("topic's commit is back", |ui| rows(ui, 5) == 3);

        ui.palette("solo in graph topic");
        ui.wait("only topic's history", |ui| {
            rows(ui, 5) == 2 && repo_settings(&repo).contains("\"solo_ref\":\"refs/heads/topic\"")
        });
    }

    /// A repository whose src/main.rs changed in three commits, plus README.md.
    fn source_history(sb: &Sandbox) -> PathBuf {
        let repo = sb.init_repo("r");
        sb.run(&repo, &["config", "user.name", "Tester"]);
        sb.run(&repo, &["config", "user.email", "tester@example.com"]);
        std::fs::create_dir_all(repo.join("src")).unwrap();
        sb.commit_file(&repo, "README.md", "readme\n", "Readme");
        for (n, text) in [
            "fn main() {}\n",
            "fn main() { run(); }\n",
            "fn main() { run(); }\nfn run() {}\n",
        ]
        .iter()
        .enumerate()
        {
            std::fs::write(repo.join("src/main.rs"), text).unwrap();
            sb.run(&repo, &["add", "."]);
            sb.run(&repo, &["commit", "-qm", &format!("Main {}", n + 1)]);
        }
        repo
    }

    fn views(ui: &mut Ui) -> Vec<String> {
        ui.read(|w, cx| w.open_views(cx))
    }

    /// The files of a commit, by keyboard: Down to src, Enter expands it, Down to main.rs;
    /// its history from the palette, Enter on a revision opens that diff, Escape comes
    /// back. (Was graphite-g15h, tree-keys and list-keys on the desktop.)
    #[test]
    fn the_tree_and_file_history_are_walked_by_keyboard() {
        let sb = Sandbox::new();
        let repo = source_history(&sb);
        let mut cx = TestAppContext::single();
        let mut ui = open(&mut cx, &sb, vec![repo.clone()]);
        ui.wait("the graph", |ui| {
            ui.window.debug_bounds("graph-row-0").is_some()
        });

        ui.palette("browse files of the checked-out commit");
        ui.wait("the tree", |ui| views(ui) == ["tree"]);
        // The rows: src/ (folders first), README.md. Down to src, Enter expands it, Down to
        // main.rs.
        ui.keys("down enter down");
        // The cursor row's menu (Shift+F10) lists its items like the palette.
        ui.keys("shift-f10");
        ui.type_text("file history");
        ui.keys("enter");
        ui.wait("the file's history", |ui| {
            views(ui).contains(&"history".to_string())
        });
        ui.keys("down enter");
        ui.wait("a revision's diff", |ui| {
            views(ui).contains(&"diff".to_string())
        });
        let history = ui.read(|w, cx| w.open_views(cx));
        assert!(history.contains(&"history".to_string()), "{history:?}");
        ui.keys("escape");
        ui.wait("back to the history", |ui| {
            !views(ui).contains(&"diff".to_string())
        });
    }

    /// The reflog from the palette: Down to HEAD@{1}, "Create branch here…" names a branch,
    /// which git puts at HEAD@{1}. (Was graphite-g15h's reflog part.)
    #[test]
    fn a_branch_made_from_a_reflog_entry_points_at_it() {
        let sb = Sandbox::new();
        let repo = source_history(&sb);
        let previous = sb.run(&repo, &["rev-parse", "HEAD@{1}"]);
        let mut cx = TestAppContext::single();
        let mut ui = open(&mut cx, &sb, vec![repo.clone()]);
        ui.wait("the graph", |ui| {
            ui.window.debug_bounds("graph-row-0").is_some()
        });

        ui.palette("show the reflog of head");
        ui.wait("the reflog", |ui| views(ui) == ["reflog"]);
        ui.keys("down down");
        // The cursor row's menu (Shift+F10), not the palette, whose "Create branch here…"
        // is the graph's selected commit.
        ui.keys("shift-f10");
        ui.type_text("create branch here");
        ui.keys("enter");
        ui.wait("the branch name dialog", |ui| ui.dialog_open());
        ui.type_text("recovered");
        ui.keys("enter");

        ui.wait("git has the branch at HEAD@{1}", |_| {
            sb.run(
                &repo,
                &[
                    "for-each-ref",
                    "--format=%(objectname)",
                    "refs/heads/recovered",
                ],
            ) == previous
        });
    }

    /// The view options from the palette narrow and widen the graph, and are saved for the
    /// repository. main has c1, c2 and a merge of topic (t1); side (s1) isn't merged.
    /// (Was graphite-graph-test.sh's view menu part.)
    #[test]
    fn the_walk_options_change_the_graph_and_are_saved() {
        let sb = Sandbox::new();
        let repo = sb.init_repo("r");
        sb.run(&repo, &["config", "user.name", "Tester"]);
        sb.run(&repo, &["config", "user.email", "tester@example.com"]);
        sb.commit_file(&repo, "a.txt", "1\n", "c1");
        for (branch, file) in [("topic", "t.txt"), ("side", "s.txt")] {
            sb.run(&repo, &["switch", "-qc", branch, "main"]);
            sb.commit_file(&repo, file, "x\n", &format!("{}1", &branch[..1]));
        }
        sb.run(&repo, &["switch", "-q", "main"]);
        sb.commit_file(&repo, "b.txt", "2\n", "c2");
        sb.run(
            &repo,
            &["merge", "-q", "--no-ff", "-m", "merge topic", "topic"],
        );
        let mut cx = TestAppContext::single();
        let mut ui = open(&mut cx, &sb, vec![repo.clone()]);
        ui.wait("every commit", |ui| rows(ui, 8) == 5);

        ui.palette("graph: show the current branch only");
        ui.wait("side's commit leaves", |ui| rows(ui, 8) == 4);
        ui.palette("graph: show first parents only");
        ui.wait("topic's commit folds into the merge", |ui| rows(ui, 8) == 3);
        ui.palette("graph: topological order");
        ui.wait("the options are saved", |_| {
            let saved = repo_settings(&repo);
            [
                "\"current_branch\":true",
                "\"first_parent\":true",
                "\"topo_order\":true",
            ]
            .iter()
            .all(|option| saved.contains(option))
        });

        ui.palette("graph: show every parent");
        ui.palette("graph: show all branches");
        ui.wait("every commit again", |ui| rows(ui, 8) == 5);
    }

    /// Clicks the element the views name `name` (a debug selector).
    fn click(ui: &mut Ui, name: &'static str) {
        let at = ui
            .window
            .debug_bounds(name)
            .unwrap_or_else(|| panic!("{name} isn't drawn"))
            .center();
        ui.window.simulate_click(at, Modifiers::none());
        ui.frame();
    }

    /// feature's review against main, as saved.
    fn review(repo: &std::path::Path) -> Option<gitweft_core::review::ReviewSession> {
        gitweft_core::review::load(&gitweft_core::review::review_file(
            repo,
            "refs/heads/feature",
        ))
    }

    /// A review from the palette's "Compare with current branch": End + Enter opens the
    /// changed file's diff, the arrows to the changed line and C ask for the text, which
    /// is saved on that line with git's user name; Mark reviewed saves the range and the
    /// file viewed. (Was graphite-review-test.sh.)
    #[test]
    fn a_review_keeps_a_line_comment_and_the_reviewed_range() {
        let sb = Sandbox::new();
        let repo = sb.init_repo("r");
        sb.run(&repo, &["config", "user.name", "Rita Reviewer"]);
        sb.run(&repo, &["config", "user.email", "rita@example.com"]);
        let lines: String = (1..=20).map(|n| format!("line {n}\n")).collect();
        sb.commit_file(&repo, "code.txt", &lines, "Start");
        sb.run(&repo, &["switch", "-qc", "feature"]);
        let changed = lines.replace("line 5\n", "line 5, reviewed\n");
        sb.commit_file(&repo, "code.txt", &changed, "Change line 5");
        sb.run(&repo, &["switch", "-q", "main"]);
        let mut cx = TestAppContext::single();
        let mut ui = open(&mut cx, &sb, vec![repo.clone()]);
        ui.wait("the graph", |ui| {
            ui.window.debug_bounds("graph-row-0").is_some()
        });

        ui.palette("compare with current branch feature");
        // Listed once its rows are read: keys before that have nothing to move to.
        ui.wait("the comparison", |ui| {
            views(ui).contains(&"compare".to_string())
        });
        ui.keys("end enter");
        ui.wait("the file's diff", |ui| {
            views(ui).contains(&"diff".to_string())
        });
        // The rows: line 1 hidden, the hunk header, lines 2–4, -line 5, +line 5 (row 6).
        ui.wait("the diff's rows", |ui| {
            ui.window.debug_bounds("diff-row-6").is_some()
        });
        ui.wait("the diff takes the keys", |ui| {
            ui.read(|w, cx| w.diff().is_some_and(|d| d.read(cx).has_focus()))
        });
        // By keyboard: the cursor to +line 5, and C comments on it; the palette
        // offers the same.
        ui.keys("home down down down down down down");
        assert!(
            labels(&mut ui).contains(&"Comment on line 5…".to_string()),
            "{:?}",
            labels(&mut ui)
        );
        ui.keys("c");
        ui.wait("the comment dialog", |ui| ui.dialog_open());
        ui.type_text("Why reviewed?");
        ui.keys("enter");

        ui.wait("the comment is saved", |_| {
            review(&repo).is_some_and(|r| r.comments.iter().any(|c| c.body == "Why reviewed?"))
        });
        let comment = review(&repo).unwrap().comments.remove(0);
        assert_eq!((comment.path.as_str(), comment.line), ("code.txt", 5));
        assert_eq!(comment.author, "Rita Reviewer");
        assert_eq!(comment.side, gitweft_core::review::Side::New);

        // The keys work again right after the dialog. C in the find box is text, not a
        // comment; Escape closes find, then the diff.
        ui.wait("the diff has the keys back", |ui| {
            ui.read(|w, cx| w.diff().is_some_and(|d| d.read(cx).has_focus()))
        });
        ui.keys("ctrl-f c");
        let query = ui.read(|w, cx| w.diff().and_then(|d| d.read(cx).find_query(cx)));
        assert_eq!(query.as_deref(), Some("c"));
        assert!(!ui.dialog_open());
        ui.keys("escape");
        ui.wait("find is closed", |ui| {
            ui.read(|w, cx| {
                w.diff()
                    .is_some_and(|d| d.read(cx).find_query(cx).is_none())
            })
        });
        ui.keys("escape");
        ui.wait("back to the comparison", |ui| views(ui) == ["compare"]);
        click(&mut ui, "mark-reviewed");
        ui.wait("the review is marked done", |_| {
            review(&repo).is_some_and(|r| r.reviewed.is_some() && r.viewed.len() == 1)
        });
    }

    /// A repository whose own config names the committer, with `commits` made in order:
    /// `(branch to switch to, or "" to stay, subject)`. Each commit adds its own file.
    fn history(sb: &Sandbox, commits: &[(&str, &str)]) -> PathBuf {
        let repo = sb.init_repo("r");
        sb.run(&repo, &["config", "user.name", "Tester"]);
        sb.run(&repo, &["config", "user.email", "tester@example.com"]);
        for (branch, subject) in commits {
            match *branch {
                "" => {}
                "main" => {
                    sb.run(&repo, &["switch", "-q", "main"]);
                }
                branch if sb.run(&repo, &["branch", "--list", branch]).is_empty() => {
                    sb.run(&repo, &["switch", "-qc", branch]);
                }
                branch => {
                    sb.run(&repo, &["switch", "-q", branch]);
                }
            }
            sb.commit_file(&repo, &format!("{subject}.txt"), "x\n", subject);
        }
        repo
    }

    /// Shift+click selects topic's three commits, Ctrl+click takes one out and puts it
    /// back; the palette's "Cherry-pick 3 commits" puts them on main oldest first, and Undo
    /// takes all three back in one step. (Was graphite-multi-pick-test.sh.)
    #[test]
    fn selected_commits_are_cherry_picked_oldest_first_and_undone_together() {
        let sb = Sandbox::new();
        let repo = history(
            &sb,
            &[
                ("", "c1"),
                ("topic", "t1"),
                ("", "t2"),
                ("", "t3"),
                ("main", "m1"),
            ],
        );
        let before = sb.run(&repo, &["rev-parse", "HEAD"]);
        let mut cx = TestAppContext::single();
        let mut ui = graph(&mut cx, &sb, &repo);
        let (t1, t2, t3) = (
            ui.row_of(&sb, &repo, "topic~2"),
            ui.row_of(&sb, &repo, "topic~1"),
            ui.row_of(&sb, &repo, "topic"),
        );
        let offers = |ui: &mut Ui, label: &str| labels(ui).iter().any(|l| l == label);

        ui.click_row(t1);
        ui.click_row_with(t3, Modifiers::shift(), 1);
        assert!(
            offers(&mut ui, "Cherry-pick 3 commits"),
            "{:?}",
            labels(&mut ui)
        );
        ui.click_row_with(t2, Modifiers::control(), 1);
        assert!(
            offers(&mut ui, "Cherry-pick 2 commits"),
            "{:?}",
            labels(&mut ui)
        );
        ui.click_row_with(t2, Modifiers::control(), 1);
        assert!(
            offers(&mut ui, "Cherry-pick 3 commits"),
            "{:?}",
            labels(&mut ui)
        );

        ui.palette("cherry-pick 3 commits");
        ui.wait("git picked them, oldest first", |_| {
            subjects(&sb, &repo) == "t3|t2|t1|m1|c1"
        });
        ui.wait("the picks can be undone", |ui| {
            labels(ui)
                .iter()
                .any(|l| l.starts_with("Undo") && l.contains("cherry-pick of 3 commits"))
        });
        ui.palette("undo");
        ui.wait("all three picks undone", |_| {
            sb.run(&repo, &["rev-parse", "HEAD"]) == before
        });
    }

    /// "Select merge base with HEAD" on side's tip selects c1, where side left main; a
    /// double click on topic's tip checks topic out. (Was graphite-graph-test.sh's parts 2
    /// and 3.)
    #[test]
    fn the_merge_base_is_selected_and_a_double_click_checks_out() {
        let sb = Sandbox::new();
        let repo = history(
            &sb,
            &[
                ("", "c1"),
                ("side", "s1"),
                ("", "s2"),
                ("main", "c2"),
                ("topic", "t1"),
                ("main", "c3"),
            ],
        );
        let c1 = commit_id(&sb, &repo, "main~2");
        let mut cx = TestAppContext::single();
        let mut ui = graph(&mut cx, &sb, &repo);

        let side = ui.row_of(&sb, &repo, "side");
        ui.click_row(side);
        ui.palette("select merge base with head");
        ui.wait("c1 is selected", |ui| ui.selected() == Some(c1));

        let topic = ui.row_of(&sb, &repo, "topic");
        ui.click_row_with(topic, Modifiers::none(), 2);
        ui.wait("git has topic checked out", |_| {
            sb.run(&repo, &["symbolic-ref", "HEAD"]) == "refs/heads/topic\n"
        });
    }

    /// Whether a process runs with `arg` in its command line.
    fn running(arg: &str) -> bool {
        std::fs::read_dir("/proc").unwrap().flatten().any(|entry| {
            std::fs::read(entry.path().join("cmdline"))
                .is_ok_and(|line| line.split(|&b| b == 0).any(|word| word == arg.as_bytes()))
        })
    }

    /// A bisect's "Run a command…" from the palette: a slow command is stopped ("Bisect:
    /// stop the command"), leaving the bisect and no process behind; then `! grep -q bug
    /// value.txt` finds c11, the first commit with the bug, and selects it in the graph.
    /// (Was graphite-g12b-test.sh's part 1.)
    #[test]
    fn a_bisect_run_can_be_stopped_and_finds_the_first_bad_commit() {
        let sb = Sandbox::new();
        let repo = sb.init_repo("r");
        sb.run(&repo, &["config", "user.name", "Tester"]);
        sb.run(&repo, &["config", "user.email", "tester@example.com"]);
        for n in 1..=12 {
            let value = if n >= 11 { "bug" } else { "fine" };
            let text = format!("step {n}\n{value}\n");
            sb.commit_file(&repo, "value.txt", &text, &format!("c{n}"));
        }
        let c11 = sb.run(&repo, &["rev-parse", "HEAD~1"]);
        sb.run(&repo, &["bisect", "start", "HEAD", "HEAD~11"]);
        let mut cx = TestAppContext::single();
        let mut ui = open(&mut cx, &sb, vec![repo.clone()]);
        let offers = |ui: &mut Ui, label: &str| labels(ui).iter().any(|l| l == label);
        ui.wait("the bisect is shown", |ui| offers(ui, "Run a command…"));

        // An odd length, so no other process has the same argument.
        let slow = "29.5";
        ui.palette("run a command");
        ui.wait("the command dialog", |ui| ui.dialog_open());
        ui.type_text(&format!("sleep {slow}"));
        ui.keys("enter");
        ui.wait("the command runs", |ui| {
            offers(ui, "Bisect: stop the command") && running(slow)
        });
        ui.palette("bisect: stop the command");
        ui.wait("the run stopped", |ui| offers(ui, "Run a command…"));
        ui.wait("its process is gone", |_| !running(slow));
        assert!(repo.join(".git/BISECT_START").exists(), "the bisect stays");

        ui.palette("run a command");
        ui.wait("the command dialog", |ui| ui.dialog_open());
        // The dialog offers the last command: replace it.
        ui.keys("ctrl-a");
        ui.type_text("! grep -q bug value.txt");
        ui.keys("enter");
        // "# first 'bad' commit: [id] c11" (older git without the quotes).
        ui.wait("git found c11", |_| {
            sb.run(&repo, &["bisect", "log"])
                .lines()
                .any(|line| line.starts_with("# first ") && line.contains(c11.trim()))
        });
        // The graph selects it too, as the details show it: not the row selected before.
        let c11 = commit_id(&sb, &repo, c11.trim());
        ui.wait("c11 is selected", |ui| ui.selected() == Some(c11));
    }

    /// "Git LFS: the largest kinds of files in this branch's history" from the palette
    /// opens the report: the kinds in plain git, one row each. Needs git-lfs.
    #[test]
    fn the_lfs_size_report_lists_the_kinds_of_files() {
        let sb = Sandbox::new();
        if sb
            .git()
            .command(sb.path())
            .args(["lfs", "version"])
            .run(&gitweft_git::CancelToken::new())
            .is_err()
        {
            eprintln!("note: git-lfs is not installed; skipping this test");
            return;
        }
        let repo = sb.init_repo("r");
        sb.commit_file(&repo, "big.dat", &"d".repeat(20_000), "data");
        sb.commit_file(&repo, "notes.txt", "short\n", "notes");
        let mut cx = TestAppContext::single();
        let mut ui = open(&mut cx, &sb, vec![repo.clone()]);
        ui.wait("the graph", |ui| {
            ui.window.debug_bounds("graph-row-0").is_some()
        });

        ui.palette("largest kinds of files in this branch");
        ui.wait("the report", |ui| {
            ui.dialog_open() && ui.window.debug_bounds("lfs-size-1").is_some()
        });
        assert!(ui.window.debug_bounds("lfs-size-2").is_none(), "two kinds");
    }

    /// A partial clone: an old commit's diff isn't downloaded by itself; the view says
    /// so, and "Fetch missing contents" gets the two versions and shows the diff.
    #[test]
    fn a_partial_clone_fetches_a_diffs_contents_only_when_asked() {
        let sb = Sandbox::new();
        // Older git fetches by itself (it ignores GIT_NO_LAZY_FETCH).
        if sb.git().version() < gitweft_git::GitVersion::new(2, 44, 0) {
            return;
        }
        let source = sb.init_repo("source");
        sb.run(&source, &["config", "uploadpack.allowFilter", "true"]);
        for text in ["one\n", "two\n", "three\n"] {
            sb.commit_file(&source, "a.txt", text, text.trim());
        }
        let url = format!("file://{}", source.display());
        sb.run(
            sb.path(),
            &["clone", "-q", "--filter=blob:none", &url, "clone"],
        );
        let clone = sb.path().join("clone");
        let (parent, commit) = (
            commit_id(&sb, &clone, "HEAD~2"),
            commit_id(&sb, &clone, "HEAD~1"),
        );
        let missing = || {
            sb.run(
                &clone,
                &["rev-list", "--objects", "--missing=print", "HEAD"],
            )
            .lines()
            .filter(|line| line.starts_with('?'))
            .count()
        };
        assert_eq!(missing(), 2);
        let mut cx = TestAppContext::single();
        let mut ui = open(&mut cx, &sb, vec![clone.clone()]);
        ui.wait("the graph", |ui| {
            ui.window.debug_bounds("graph-row-0").is_some()
        });

        let workspace = ui.workspace();
        ui.window.update(|_, cx| {
            workspace.update(cx, |w, cx| {
                w.open_commit_diff(Some(parent), commit, "a.txt", cx)
            })
        });
        ui.wait("the offer to fetch", |ui| {
            ui.window.debug_bounds("fetch-missing").is_some()
        });
        assert_eq!(missing(), 2, "nothing downloaded by the read");

        click(&mut ui, "fetch-missing");
        ui.wait("the diff's rows", |ui| {
            ui.window.debug_bounds("diff-row-1").is_some()
        });
        assert_eq!(missing(), 0);
    }

    /// A view that had the keys and closed, but lives on a moment longer (as when a
    /// palette command still holds it): the keys don't stay with it, so Ctrl+Shift+P
    /// still opens the palette.
    #[test]
    fn the_keys_leave_a_closed_view_that_lives_on() {
        let sb = Sandbox::new();
        let repo = four_commits(&sb);
        let (parent, commit) = (
            commit_id(&sb, &repo, "HEAD~1"),
            commit_id(&sb, &repo, "HEAD"),
        );
        let mut cx = TestAppContext::single();
        let mut ui = graph(&mut cx, &sb, &repo);
        let workspace = ui.workspace();
        ui.window.update(|_, cx| {
            workspace.update(cx, |w, cx| {
                w.open_commit_diff(Some(parent), commit, "f4.txt", cx)
            })
        });
        ui.wait("the diff's rows", |ui| {
            ui.window.debug_bounds("diff-row-1").is_some()
        });
        let diff = ui.read(|w, _| w.diff()).expect("the diff is open");
        let focus = ui.window.update(|_, cx| diff.read(cx).focus_handle(cx));
        ui.window.update(|window, cx| focus.focus(window, cx));
        ui.frame();

        ui.keys("escape");
        ui.wait("the diff closed", |ui| ui.read(|w, _| w.diff().is_none()));
        // `diff` keeps the view alive, as a palette command can.
        ui.keys("ctrl-shift-p");
        assert!(ui.dialog_open(), "the palette opened");
        drop(diff);
    }

    /// Three toasts in a row are all on screen (gpui-kit's stack showed only the
    /// newest); closing one leaves the others.
    #[test]
    fn toasts_in_a_row_are_all_shown() {
        let sb = Sandbox::new();
        let repo = four_commits(&sb);
        let mut cx = TestAppContext::single();
        let mut ui = open(&mut cx, &sb, vec![repo]);
        ui.window.update(|_, cx| {
            crate::toast::push(crate::toast::Toast::success("first"), cx);
            crate::toast::push(crate::toast::Toast::error("second").title("Title"), cx);
            crate::toast::push(crate::toast::Toast::info("third"), cx);
        });
        ui.frame();
        for name in ["toast-0", "toast-1", "toast-2"] {
            assert!(ui.window.debug_bounds(name).is_some(), "{name}");
        }
        let (first, third) = (
            ui.window.debug_bounds("toast-0").unwrap(),
            ui.window.debug_bounds("toast-2").unwrap(),
        );
        assert!(
            first.bottom() <= third.top(),
            "stacked, newest at the bottom"
        );
    }

    /// The side panels. Dragged by their dividers they have only minimums: the
    /// sidebar goes past the old 520 px maximum and stops where the centre is at its minimum,
    /// the other panel never changes; the widths are saved in design pixels, so the zoom
    /// scales them.
    #[test]
    fn the_side_panels_have_only_minimums_and_follow_the_zoom() {
        let sb = Sandbox::new();
        let repo = four_commits(&sb);
        let mut cx = TestAppContext::single();
        let mut ui = open(&mut cx, &sb, vec![repo]);
        ui.wait("the graph", |ui| {
            ui.window.debug_bounds("graph-row-0").is_some()
        });
        let width = |ui: &mut Ui, name: &'static str| {
            f32::from(ui.window.debug_bounds(name).expect(name).size.width).round()
        };
        let widths = |ui: &mut Ui| (width(ui, "panel-sidebar"), width(ui, "panel-right"));
        let saved = |ui: &mut Ui| {
            ui.window.update(|_, cx| {
                let layout = &SettingsStore::settings(cx).layout;
                (layout.sidebar_width, layout.right_panel_width)
            })
        };
        let drag = |ui: &mut Ui, grab: &'static str, x: f32| {
            let from = ui.window.debug_bounds(grab).expect(grab).center();
            let to = point(px(x), from.y);
            let (left, none) = (MouseButton::Left, Modifiers::none());
            // Each step on a fresh frame: GPUI starts a drag past a few pixels' move,
            // from what the last frame drew.
            ui.window.simulate_mouse_move(from, None, none);
            ui.frame();
            ui.window.simulate_mouse_down(from, left, none);
            ui.frame();
            ui.window
                .simulate_mouse_move(from + point(px(8.), px(0.)), left, none);
            ui.frame();
            ui.window.simulate_mouse_move(to, left, none);
            ui.frame();
            ui.window.simulate_mouse_up(to, left, none);
            ui.frame();
        };
        assert_eq!(widths(&mut ui), (256., 340.));
        let window = f32::from(ui.window.debug_bounds("panel-centre").unwrap().right()) + 1. + 340.;

        // Past the old maximum; the right panel keeps its width.
        drag(&mut ui, "divider-sidebar-grab", 700.);
        assert_eq!(widths(&mut ui), (700., 340.));
        assert_eq!(saved(&mut ui), (Some(700.), Some(340.)));

        // As far as it goes: the centre keeps its minimum, the right panel its width.
        drag(&mut ui, "divider-sidebar-grab", window - 10.);
        assert_eq!(width(&mut ui, "panel-centre"), 360.);
        assert_eq!(width(&mut ui, "panel-right"), 340.);

        // The right divider, as far left as it goes: the sidebar doesn't move.
        drag(&mut ui, "divider-sidebar-grab", 300.);
        drag(&mut ui, "divider-right-grab", 10.);
        assert_eq!(width(&mut ui, "panel-sidebar"), 300.);
        assert_eq!(width(&mut ui, "panel-centre"), 360.);

        // The zoom scales the saved widths; a drag at 150 % saves design pixels.
        let zoom = |ui: &mut Ui, zoom: f32| {
            ui.window.update(|_, cx| {
                SettingsStore::update(cx, |settings| {
                    settings.appearance.zoom = zoom;
                    settings.layout.right_panel_width = Some(340.);
                })
            });
            ui.frame();
        };
        zoom(&mut ui, 1.5);
        assert_eq!(widths(&mut ui), (450., 510.));
        drag(&mut ui, "divider-sidebar-grab", 300.);
        assert_eq!(
            saved(&mut ui).0,
            Some(200.),
            "300 px at 150 % is 200 design pixels"
        );
        zoom(&mut ui, 1.0);
        assert_eq!(widths(&mut ui), (200., 340.));

        // A window too narrow for both and the centre's minimum: the right panel gives way
        // first (down to its minimum); the saved widths stay, and come back with the room.
        ui.window.simulate_resize(size(px(800.), px(900.)));
        ui.frame();
        let (sidebar, right) = widths(&mut ui);
        assert_eq!(right, 260.);
        assert!(sidebar < 200., "then the sidebar: {sidebar}");
        assert_eq!(saved(&mut ui), (Some(200.), Some(340.)));
        ui.window.simulate_resize(size(px(1920.), px(900.)));
        ui.frame();
        assert_eq!(widths(&mut ui), (200., 340.));
    }

    /// Drags `grab` (a divider's debug selector) by `dy` pixels, a frame between each step:
    /// GPUI starts a drag past a few pixels' move, from what the last frame drew.
    fn drag_by(ui: &mut Ui, grab: &'static str, dx: f32, dy: f32) {
        let from = ui.window.debug_bounds(grab).expect(grab).center();
        let to = from + point(px(dx), px(dy));
        let (left, none) = (MouseButton::Left, Modifiers::none());
        ui.window.simulate_mouse_move(from, None, none);
        ui.frame();
        ui.window.simulate_mouse_down(from, left, none);
        ui.frame();
        let nudge = if dx == 0. {
            point(px(0.), px(8.) * dy.signum())
        } else {
            point(px(8.) * dx.signum(), px(0.))
        };
        ui.window.simulate_mouse_move(from + nudge, left, none);
        ui.frame();
        ui.window.simulate_mouse_move(to, left, none);
        ui.frame();
        ui.window.simulate_mouse_up(to, left, none);
        ui.frame();
    }

    /// Waits until `name`'s bounds hold still from one frame to the next. A panel's first
    /// frames size it on a guess, before it has been measured, and a drag from those
    /// bounds starts where nothing is any more.
    fn settled(ui: &mut Ui, name: &'static str) {
        let mut last = None;
        ui.wait(&format!("{name} holds still"), |ui| {
            let now = ui.window.debug_bounds(name);
            let still = now.is_some() && now == last;
            last = now;
            still
        });
    }

    fn height(ui: &mut Ui, name: &'static str) -> f32 {
        f32::from(ui.window.debug_bounds(name).expect(name).size.height).round()
    }

    /// The right panel's sections: the line between Unstaged and Staged, the commit
    /// box's top, a commit's message and facts and the file preview are dragged; the sizes
    /// are saved (to the file too), and a double click puts one back.
    #[test]
    fn the_right_panels_sections_are_resized_and_kept() {
        let sb = Sandbox::new();
        let repo = sb.init_repo("r");
        let body: String = (1..=12).map(|n| format!("Body line {n}.\n")).collect();
        std::fs::write(repo.join("a.txt"), "one\n").unwrap();
        sb.run(&repo, &["add", "a.txt"]);
        sb.run(&repo, &["commit", "-qm", &format!("A commit\n\n{body}")]);
        for n in 0..40 {
            std::fs::write(repo.join(format!("staged{n:02}.txt")), "new\n").unwrap();
        }
        sb.run(&repo, &["add", "."]);
        for n in 0..40 {
            std::fs::write(repo.join(format!("unstaged{n:02}.txt")), "new\n").unwrap();
        }
        let mut cx = TestAppContext::single();
        let mut ui = staging_keys(&mut cx, &sb, &repo, 80);
        let layout = |ui: &mut Ui| {
            ui.window
                .update(|_, cx| SettingsStore::settings(cx).layout.clone())
        };
        let (unstaged, staged) = (
            height(&mut ui, "staging-unstaged-list"),
            height(&mut ui, "staging-staged-list"),
        );
        assert_eq!(unstaged, staged, "halves by default");

        drag_by(&mut ui, "split-lists", 0., 100.);
        let (unstaged, staged) = (
            height(&mut ui, "staging-unstaged-list"),
            height(&mut ui, "staging-staged-list"),
        );
        assert!(unstaged > staged + 150., "{unstaged} over {staged}");
        let share = layout(&mut ui).staging_split.expect("saved");
        assert!(
            (share - unstaged / (unstaged + staged)).abs() < 0.02,
            "{share}"
        );

        let commit_box = height(&mut ui, "commit-box");
        drag_by(&mut ui, "split-commit-box", 0., -120.);
        assert_eq!(height(&mut ui, "commit-box"), commit_box + 120.);
        assert_eq!(layout(&mut ui).commit_box_height, Some(commit_box + 120.));
        // Written to the settings file, so the next start has them.
        // (after the save's delay, on the test's clock).
        let file = sb.path().join("settings.json");
        ui.window.executor().advance_clock(Duration::from_secs(1));
        ui.wait("the file has them", |_| {
            std::fs::read_to_string(&file).is_ok_and(|text| text.contains("commit_box_height"))
        });

        // A double click puts the Unstaged/Staged line back.
        let at = ui.window.debug_bounds("split-lists").unwrap().center();
        for click_count in 1..=2 {
            let (button, modifiers) = (MouseButton::Left, Modifiers::none());
            ui.window.simulate_event(MouseDownEvent {
                position: at,
                modifiers,
                button,
                click_count,
                first_mouse: false,
            });
            ui.window.simulate_event(MouseUpEvent {
                position: at,
                modifiers,
                button,
                click_count,
            });
            ui.frame();
        }
        assert_eq!(layout(&mut ui).staging_split, None);

        // A commit's details: the message and facts, then the file preview.
        ui.click_row(0);
        ui.wait("the details", |ui| {
            ui.window.debug_bounds("split-details-top").is_some()
        });
        settled(&mut ui, "details-top");
        let top = height(&mut ui, "details-top");
        drag_by(&mut ui, "split-details-top", 0., 60.);
        assert_eq!(height(&mut ui, "details-top"), top + 60.);
        let saved = layout(&mut ui).details_top_height.expect("saved");
        assert!((saved - (top + 60.)).abs() < 1., "{saved}");

        // A file selected by key: its preview, whose share is dragged.
        let focus = ui.read(|w, cx| w.details().read(cx).focus_handle(cx));
        ui.window.update(|window, cx| focus.focus(window, cx));
        ui.keys("down");
        ui.wait("the preview", |ui| {
            ui.window.debug_bounds("split-preview").is_some()
        });
        let above = ui.window.debug_bounds("split-preview").unwrap().center().y;
        drag_by(&mut ui, "split-preview", 0., -80.);
        let moved = ui.window.debug_bounds("split-preview").unwrap().center().y;
        assert!(
            (f32::from(above - moved) - 80.).abs() < 1.,
            "moved by {:?}",
            above - moved
        );
        assert!(layout(&mut ui).preview_share.is_some());
    }

    /// Cut text shows its whole text on hover: a long branch name in its graph pill
    /// is cut and has the tooltip; a short one is whole.
    #[test]
    fn a_cut_branch_name_is_known_to_be_cut_and_a_short_one_is_not() {
        let sb = Sandbox::new();
        let repo = four_commits(&sb);
        let long = "feature/an-extremely-long-branch-name-that-no-pill-can-show-whole";
        sb.run(&repo, &["branch", long, "HEAD~2"]);
        sb.run(&repo, &["branch", "short", "HEAD~1"]);
        let mut cx = TestAppContext::single();
        let mut ui = open(&mut cx, &sb, vec![repo]);
        ui.wait("the graph", |ui| {
            ui.window.debug_bounds("graph-row-1").is_some()
        });
        // The first frame measures; the next one has the result.
        ui.frame();
        let cut: &'static str = Box::leak(format!("cut:{long}").into_boxed_str());
        assert!(
            ui.window.debug_bounds(cut).is_some(),
            "the long name is cut"
        );
        assert!(
            ui.window.debug_bounds("whole:short").is_some(),
            "the short one isn't"
        );
        assert!(ui.window.debug_bounds("cut:short").is_none());

        // Hovered, the cut one shows its tooltip (on the test's clock).
        let at = ui.window.debug_bounds(cut).unwrap().center();
        ui.window.simulate_mouse_move(at, None, Modifiers::none());
        ui.frame();
        ui.window.executor().advance_clock(Duration::from_secs(2));
        ui.frame();
        let tip: &'static str = Box::leak(format!("tooltip:{long}").into_boxed_str());
        assert!(
            ui.window.debug_bounds(tip).is_some(),
            "the whole name in a tooltip"
        );
    }

    /// The settings window: each sidebar entry shows its own page. Theme
    /// and Text differ; Colors sits under Appearance, and Saved colors, Surfaces and Saved
    /// colors again each show their own; a search finds a setting on any page.
    #[test]
    fn each_settings_entry_shows_its_own_page() {
        let sb = Sandbox::new();
        let cx = TestAppContext::single();
        cx.update(|cx| {
            gpui_kit::init(cx);
            SettingsStore::init_with_file(cx, &sb.path().join("settings.json"));
            crate::keymap::KeymapStore::init_with_file(cx, sb.path().join("keymap.json"));
            crate::bind_keys(cx);
            crate::settings_window::open(cx);
        });
        let window = cx.windows()[0];
        let mut settings = VisualTestContext::from_window(window, &cx);
        let frame = |settings: &mut VisualTestContext| {
            settings.run_until_parked();
            settings.update(|window, cx| window.draw(cx).clear(cx));
            settings.run_until_parked();
        };
        let click = |settings: &mut VisualTestContext, name: &'static str| {
            let at = settings.debug_bounds(name).expect(name).center();
            settings.simulate_click(at, Modifiers::none());
            frame(settings);
        };
        let shows = |settings: &mut VisualTestContext, page: &'static str| {
            settings.debug_bounds(page).is_some()
        };
        frame(&mut settings);
        assert!(
            shows(&mut settings, "settings-page-General"),
            "General first"
        );
        assert!(
            settings.debug_bounds("settings-nav-Theme").is_none(),
            "Appearance closed"
        );

        // Appearance opens on Theme; Text shows its own page.
        click(&mut settings, "settings-nav-Appearance");
        assert!(shows(&mut settings, "settings-page-Theme"));
        click(&mut settings, "settings-nav-Text");
        assert!(shows(&mut settings, "settings-page-Text"));
        assert!(
            !shows(&mut settings, "settings-page-Theme"),
            "not Theme's content"
        );

        // Colors under Appearance, and its own pages.
        click(&mut settings, "settings-nav-Colors");
        assert!(shows(&mut settings, "settings-page-Colors"));
        assert!(
            settings.debug_bounds("settings-nav-Theme").is_some(),
            "still under Appearance"
        );
        for page in ["Saved colors", "Surfaces", "Saved colors"] {
            let nav: &'static str = Box::leak(format!("settings-nav-{page}").into_boxed_str());
            let shown: &'static str = Box::leak(format!("settings-page-{page}").into_boxed_str());
            click(&mut settings, nav);
            assert!(shows(&mut settings, shown), "{page} shows its page");
        }
        assert!(!shows(&mut settings, "settings-page-Surfaces"));

        // A search finds a setting wherever it is.
        settings.update(|window, cx| crate::settings_window::search_for("density", window, cx));
        frame(&mut settings);
        let found = "settings-found-Appearance › Theme";
        assert!(
            settings.debug_bounds(found).is_some(),
            "found on Appearance › Theme"
        );
    }

    /// Tab to a control below a settings page's fold scrolls the page to it. On the
    /// Keyboard page, Tab walks the commands' Record and Remove buttons down past the
    /// window's bottom; the last command's row must come into view on the way. The page is
    /// scrolled only when the focus enters a row, so the wheel can still move it after.
    #[test]
    fn tab_to_a_setting_below_the_fold_scrolls_the_page_to_it() {
        let sb = Sandbox::new();
        let cx = TestAppContext::single();
        cx.update(|cx| {
            gpui_kit::init(cx);
            SettingsStore::init_with_file(cx, &sb.path().join("settings.json"));
            crate::keymap::KeymapStore::init_with_file(cx, sb.path().join("keymap.json"));
            crate::bind_keys(cx);
            crate::settings_window::open(cx);
        });
        let window = cx.windows()[0];
        let mut settings = VisualTestContext::from_window(window, &cx);
        let frame = |settings: &mut VisualTestContext| {
            settings.run_until_parked();
            settings.update(|window, cx| window.draw(cx).clear(cx));
            settings.run_until_parked();
        };
        frame(&mut settings);
        let at = settings
            .debug_bounds("settings-nav-Keyboard")
            .unwrap()
            .center();
        settings.simulate_click(at, Modifiers::none());
        frame(&mut settings);

        let commands = crate::keymap::bindable();
        let last = crate::keymap::GROUPS
            .iter()
            .rev()
            .find_map(|group| commands.iter().rfind(|command| command.group == *group))
            .unwrap();
        let row: &'static str = Box::leak(format!("settings-row-{}", last.label).into_boxed_str());
        let shown = |settings: &mut VisualTestContext| {
            let page = settings.debug_bounds("settings-page").unwrap();
            let row = settings.debug_bounds(row).expect(row);
            row.top() >= page.top() && row.bottom() <= page.bottom()
        };
        assert!(
            !shown(&mut settings),
            "the fixture's last command starts below the fold"
        );
        // From the search box, as a keyboard user starts.
        let at = settings.debug_bounds("settings-search").unwrap().center();
        settings.simulate_click(at, Modifiers::none());
        frame(&mut settings);

        let mut presses = 0;
        while !shown(&mut settings) {
            presses += 1;
            assert!(
                presses <= 2 * commands.len() + 40,
                "Tab never brought {row} into view"
            );
            settings.simulate_keystrokes("tab");
            frame(&mut settings);
        }

        // The focus is still in that row; the wheel moves the page away from it all the same.
        let page = settings.debug_bounds("settings-page").unwrap();
        settings.simulate_event(ScrollWheelEvent {
            position: page.center(),
            delta: ScrollDelta::Pixels(point(px(0.), px(4000.))),
            ..Default::default()
        });
        frame(&mut settings);
        frame(&mut settings);
        assert!(
            !shown(&mut settings),
            "the wheel isn't undone by the reveal"
        );
    }

    /// A commit's message and facts take the room they need, up to 60 % of the details
    /// panel: a short message with branches shows its refs row whole, not under the
    /// fold; a long body stops at 60 % of the panel under its header.
    #[test]
    fn the_details_top_is_as_tall_as_it_needs_up_to_sixty_percent() {
        let sb = Sandbox::new();
        let repo = sb.init_repo("r");
        sb.commit_file(&repo, "a.txt", "one\n", "A short message");
        sb.run(&repo, &["branch", "feature"]);
        sb.run(&repo, &["tag", "v1"]);
        let mut cx = TestAppContext::single();
        let mut ui = open(&mut cx, &sb, vec![repo.clone()]);
        ui.wait("the details with its refs", |ui| {
            ui.window.debug_bounds("details-refs").is_some()
        });
        ui.frame();
        let top = ui.window.debug_bounds("details-top").unwrap();
        let refs = ui.window.debug_bounds("details-refs").unwrap();
        assert!(
            refs.bottom() <= top.bottom(),
            "the refs row {refs:?} lies inside the details' visible part {top:?}"
        );

        // A long body: 60 % of the panel under its header.
        let body: String = (1..=80).map(|n| format!("Body line {n}.\n")).collect();
        std::fs::write(repo.join("a.txt"), "two\n").unwrap();
        sb.run(&repo, &["commit", "-qam", &format!("A long one\n\n{body}")]);
        ui.wait("the new commit in the graph", |ui| {
            ui.window.debug_bounds("graph-row-1").is_some()
        });
        ui.click_row(0);
        // Its body is cut to a few lines until "Show more".
        ui.wait("its Show more", |ui| {
            ui.window.debug_bounds("details-body-toggle").is_some()
        });
        let at = ui
            .window
            .debug_bounds("details-body-toggle")
            .unwrap()
            .center();
        ui.window.simulate_click(at, Modifiers::none());
        ui.frame();
        ui.frame();
        let panel = ui.window.debug_bounds("details-panel").unwrap();
        let top = ui.window.debug_bounds("details-top").unwrap();
        let room = f32::from(panel.bottom() - top.top());
        let height = f32::from(top.size.height);
        assert!(
            (height - room * 0.6).abs() < 3.,
            "{height} is 60 % of the {room} px under the header"
        );
    }

    /// Identity profiles: this repository's identity saved as "Home" from the
    /// palette; a "Work" profile applied puts its name, email and key in the repository's
    /// own config.
    #[test]
    fn identity_profiles_are_saved_and_applied_per_repository() {
        let sb = Sandbox::new();
        let repo = four_commits(&sb);
        let mut cx = TestAppContext::single();
        let mut ui = open(&mut cx, &sb, vec![repo.clone()]);
        ui.wait("the identity is read", |ui| {
            ui.read(|w, cx| w.staging().read(cx).identity().email.is_some())
        });
        let profiles = |ui: &mut Ui| {
            ui.window.update(|_, cx| {
                SettingsStore::settings(cx)
                    .identity_profiles
                    .iter()
                    .map(|p| (p.label.clone(), p.email.clone()))
                    .collect::<Vec<_>>()
            })
        };

        ui.palette("save this repository's identity as a profile");
        ui.wait("the label dialog", |ui| ui.dialog_open());
        ui.type_text("Home");
        ui.keys("enter");
        ui.wait("the profile is kept", |ui| !profiles(ui).is_empty());
        assert_eq!(
            profiles(&mut ui),
            [("Home".to_owned(), "tester@example.com".to_owned())]
        );

        ui.window.update(|_, cx| {
            SettingsStore::update(cx, |settings| {
                settings
                    .identity_profiles
                    .push(gitweft_core::settings::IdentityProfile {
                        label: "Work".into(),
                        name: "Ada Work".into(),
                        email: "ada@work.example".into(),
                        signing_key: Some("KEY1".into()),
                    })
            })
        });
        ui.palette("use identity work in this repository");
        // The three keys are written one after another, and a key not yet set makes
        // `git config` fail: wait for all of them.
        let config = |key: &str| {
            sb.git()
                .command(&repo)
                .args(["config", "--local", key])
                .run(&gitweft_git::CancelToken::new())
                .map(|out| out.stdout_str().trim().to_owned())
                .unwrap_or_default()
        };
        ui.wait("git has the profile", |_| {
            config("user.signingkey") == "KEY1"
        });
        assert_eq!(config("user.name"), "Ada Work");
        assert_eq!(config("user.email"), "ada@work.example");
    }

    /// Git LFS migrate on the checked-out branch: from the palette, the patterns,
    /// "Rewrite" confirmed; git has the file in LFS, and Undo takes the branch back.
    #[test]
    fn a_branch_is_moved_into_lfs_and_undone() {
        let sb = Sandbox::new();
        if sb
            .git()
            .command(sb.path())
            .args(["lfs", "version"])
            .run(&gitweft_git::CancelToken::new())
            .is_err()
        {
            eprintln!("note: git-lfs is not installed; skipping this test");
            return;
        }
        let repo = four_commits(&sb);
        sb.run(&repo, &["lfs", "install", "--local"]);
        sb.commit_file(&repo, "big.bin", &"b".repeat(5_000), "data");
        let before = sb.run(&repo, &["rev-parse", "HEAD"]);
        let mut cx = TestAppContext::single();
        let mut ui = graph(&mut cx, &sb, &repo);
        ui.wait("the status is read", |ui| {
            ui.read(|w, cx| w.staging().read(cx).status().unstaged().count() == 0)
        });

        ui.palette("move files into lfs on this branch");
        ui.wait("the patterns dialog", |ui| ui.dialog_open());
        ui.type_text("*.bin");
        ui.keys("enter");
        ui.answer_prompt("Rewrite");
        ui.wait("git keeps big.bin in LFS", |_| {
            sb.run(&repo, &["lfs", "ls-files"]).contains("big.bin")
        });
        assert_ne!(sb.run(&repo, &["rev-parse", "HEAD"]), before);

        ui.wait("the rewrite can be undone", |ui| {
            labels(ui)
                .iter()
                .any(|l| l.starts_with("Undo") && l.contains("into Git LFS"))
        });
        ui.palette("undo");
        ui.wait("the branch is back", |_| {
            sb.run(&repo, &["rev-parse", "HEAD"]) == before
        });
    }

    /// The file history's row menu opens the file as of that commit.
    #[test]
    fn a_history_row_opens_the_file_as_of_its_commit() {
        let sb = Sandbox::new();
        let repo = source_history(&sb);
        let mut cx = TestAppContext::single();
        let mut ui = open(&mut cx, &sb, vec![repo.clone()]);
        ui.wait("the graph", |ui| {
            ui.window.debug_bounds("graph-row-0").is_some()
        });
        ui.palette("browse files of the checked-out commit");
        ui.wait("the tree", |ui| views(ui) == ["tree"]);
        ui.keys("down enter down shift-f10");
        ui.type_text("file history");
        ui.keys("enter");
        ui.wait("the file's history", |ui| {
            views(ui).contains(&"history".to_string())
        });
        ui.keys("down shift-f10");
        ui.type_text("open the file as of this commit");
        ui.keys("enter");
        ui.wait("the file view", |ui| {
            views(ui).iter().any(|view| view.starts_with("file "))
        });
    }

    /// A comparison's commit row has the graph's items: its menu cherry-picks it
    /// onto the checked-out branch.
    #[test]
    fn a_compared_commit_is_cherry_picked_from_its_row_menu() {
        let sb = Sandbox::new();
        let repo = four_commits(&sb);
        sb.run(&repo, &["switch", "-qc", "feature"]);
        sb.commit_file(&repo, "feature.txt", "x\n", "Feature work");
        sb.run(&repo, &["switch", "-q", "main"]);
        let mut cx = TestAppContext::single();
        let mut ui = graph(&mut cx, &sb, &repo);

        ui.palette("compare with current branch feature");
        ui.wait("the comparison", |ui| {
            views(ui).contains(&"compare".to_string())
        });
        // The first stop is the feature's commit.
        ui.keys("down shift-f10");
        ui.type_text("cherry-pick commit");
        ui.keys("enter");
        ui.wait("git picked it onto main", |_| {
            subjects(&sb, &repo).starts_with("Feature work|c4")
        });
    }

    /// A partial clone's blame and old file version: each says the contents are
    /// missing and fetches them only when asked, then shows.
    #[test]
    fn a_partial_clone_fetches_blame_and_file_contents_only_when_asked() {
        let sb = Sandbox::new();
        if sb.git().version() < gitweft_git::GitVersion::new(2, 44, 0) {
            return;
        }
        let source = sb.init_repo("source");
        sb.run(&source, &["config", "uploadpack.allowFilter", "true"]);
        for text in ["one\n", "two\n", "three\n"] {
            sb.commit_file(&source, "a.txt", text, text.trim());
        }
        let url = format!("file://{}", source.display());
        sb.run(
            sb.path(),
            &["clone", "-q", "--filter=blob:none", &url, "clone"],
        );
        let clone = sb.path().join("clone");
        let missing = || {
            sb.run(
                &clone,
                &["rev-list", "--objects", "--missing=print", "HEAD"],
            )
            .lines()
            .filter(|line| line.starts_with('?'))
            .count()
        };
        let mut cx = TestAppContext::single();
        let mut ui = open(&mut cx, &sb, vec![clone.clone()]);
        ui.wait("the graph", |ui| {
            ui.window.debug_bounds("graph-row-0").is_some()
        });
        let workspace = ui.workspace();

        // The file as of HEAD~2: only that version is fetched.
        let old = commit_id(&sb, &clone, "HEAD~2");
        ui.window.update(|_, cx| {
            workspace.update(cx, |w, cx| w.open_blame_or_file(Some(old), "a.txt", cx))
        });
        ui.wait("the file's offer", |ui| {
            ui.window.debug_bounds("fetch-missing-file").is_some()
        });
        assert_eq!(missing(), 2);
        click(&mut ui, "fetch-missing-file");
        ui.wait("the file", |_| missing() == 1);
        ui.wait("the file view", |ui| {
            ui.window.debug_bounds("fetch-missing-file").is_none()
                && views(ui).iter().any(|view| view.starts_with("file "))
        });

        // Blame at HEAD needs the rest of the history.
        ui.window
            .update(|_, cx| workspace.update(cx, |w, cx| w.open_blame_or_file(None, "a.txt", cx)));
        ui.wait("blame's offer", |ui| {
            ui.window.debug_bounds("fetch-missing-blame").is_some()
        });
        click(&mut ui, "fetch-missing-blame");
        ui.wait("every version fetched", |_| missing() == 0);
        ui.wait("the blame without the offer", |ui| {
            ui.window.debug_bounds("fetch-missing-blame").is_none()
        });
    }
}
