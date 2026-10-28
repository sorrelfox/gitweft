//! The user's other programs for the repository and its files: a terminal, the file manager
//! and an editor (`gitweft_core::external`), and git's diff and merge tools
//! (`gitweft_git::tools`). Each runs on a thread of its own: a diff tool holds its thread
//! until it's closed, which must not take a worker of the pool.

use gitweft_core::external::{self as programs, Environment, LaunchError};
use gitweft_git::gix::bstr::BStr;
use gitweft_git::tools::{self, DiffToolTarget, ToolError, ToolKind};

use crate::staging_view::FileTool;

use super::*;

/// The program a folder or file opens in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Program {
    Terminal,
    FileManager,
    Editor,
}

impl Program {
    fn failed_title(self) -> String {
        match self {
            Program::Terminal => t!("external.terminal_failed"),
            Program::FileManager => t!("external.file_manager_failed"),
            Program::Editor => t!("external.editor_failed"),
        }
        .into_owned()
    }
}

/// Runs `work` on a thread of its own; its result arrives on the channel (which closes
/// without one if the thread couldn't start).
fn on_thread<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> async_channel::Receiver<T> {
    let (tx, rx) = async_channel::bounded(1);
    let started = std::thread::Builder::new()
        .name("gitweft-external".into())
        .spawn(move || {
            let _ = tx.send_blocking(work());
        });
    if let Err(err) = started {
        eprintln!("gitweft: couldn't start a thread: {err}");
    }
    rx
}

/// A launch failure in the user's language (a program's own message stays as it is).
fn launch_message(err: &LaunchError) -> String {
    match err {
        LaunchError::BadCommand(command) => t!("external.bad_command", command = command),
        LaunchError::NotFound(program) => t!("external.not_found", program = program),
        LaunchError::NoTerminal => t!("external.no_terminal"),
        LaunchError::NoCommandForm(terminal) => t!("external.no_command_form", terminal = terminal),
        LaunchError::Failed { program, message } => {
            t!(
                "external.failed_detail",
                program = program,
                message = message
            )
        }
    }
    .into_owned()
}

fn tool_message(err: &ToolError) -> String {
    match err {
        ToolError::NotConfigured(ToolKind::Diff) => t!("external.no_diff_tool").into_owned(),
        ToolError::NotConfigured(ToolKind::Merge) => t!("external.no_merge_tool").into_owned(),
        ToolError::NoHostGit => t!("external.no_host_git").into_owned(),
        ToolError::MergeToolMissing(tool) => {
            t!("external.merge_tool_missing", tool = tool).into_owned()
        }
        ToolError::Git(err) => err.summary(),
    }
}

impl Workspace {
    /// The open repository's folder: its working tree, or its git dir when bare.
    fn repository_folder(&self) -> Option<PathBuf> {
        let RepoState::Loaded(repo) = &self.state else {
            return None;
        };
        Some(repo.cwd())
    }

    /// Opens `path` in `program`: a terminal in that folder, the file manager showing it (a
    /// file selected in its folder), or the editor with that file. A program that doesn't
    /// start shows a notification.
    pub(crate) fn open_in(
        &mut self,
        program: Program,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let tools = SettingsStore::settings(cx).tools.clone();
        let workdir = self.repository_folder().unwrap_or_else(|| path.clone());
        let result = on_thread(move || {
            let env = Environment::current();
            let launch = match program {
                Program::Terminal => programs::terminal(&tools.terminal, &path, &env),
                Program::FileManager => {
                    programs::file_manager(&tools.file_manager, &path, path.is_dir(), &env)
                }
                Program::Editor => programs::editor(&tools.editor, &path, &workdir, &env),
            }?;
            programs::launch(&launch)
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Err(err)) = result.recv().await {
                this.update_in(cx, |_, _, cx| {
                    let message = Toast::error(launch_message(&err));
                    note(message.title(program.failed_title()), cx)
                })
                .ok();
            }
        })
        .detach();
    }

    /// The repository's folder in a terminal or the file manager (the More menu, the
    /// palette).
    pub(crate) fn open_repository_in(
        &mut self,
        program: Program,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(folder) = self.repository_folder() {
            self.open_in(program, folder, window, cx);
        }
    }

    /// A file of the working tree (its repository path) in the editor or file manager.
    pub(crate) fn open_working_file(
        &mut self,
        path: &[u8],
        program: Program,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(folder) = self.repository_folder() {
            let file = folder.join(gitweft_platform::bytes::path_from_bytes(path));
            self.open_in(program, file, window, cx);
        }
    }

    /// A changed file's other programs (its menu in the staging list and the palette).
    pub(crate) fn open_file_tool(
        &mut self,
        entry: StatusEntry,
        tool: FileTool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match tool {
            FileTool::DiffTool => {
                let target = self.working_tree_target();
                self.run_diff_tool(target, entry.path, entry.orig_path, window, cx)
            }
            FileTool::MergeTool => self.run_merge_tool(entry.path, window, cx),
            FileTool::Editor => self.open_working_file(&entry.path, Program::Editor, window, cx),
            FileTool::Folder => {
                self.open_working_file(&entry.path, Program::FileManager, window, cx)
            }
            FileTool::LfsLock => self.run_lfs_lock_op(entry.path, true, false, window, cx),
            FileTool::LfsUnlock => self.run_lfs_lock_op(entry.path, false, false, window, cx),
        }
    }

    /// HEAD (the empty tree on an unborn branch) against the working tree.
    fn working_tree_target(&self) -> DiffToolTarget {
        DiffToolTarget::WorkingTree {
            base: self.status.as_ref().and_then(|status| status.branch.oid),
        }
    }

    /// `git difftool` for one file (`old_path`: its name before a rename). It runs until the
    /// tool is closed; a failure shows a notification.
    pub(crate) fn run_diff_tool(
        &mut self,
        target: DiffToolTarget,
        path: BString,
        old_path: Option<BString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (RepoState::Loaded(repo), Some(cwd)) = (&self.state, self.repository_folder()) else {
            return;
        };
        let git = repo.git.clone();
        let tool = SettingsStore::settings(cx)
            .tools
            .diff_tool
            .trim()
            .to_owned();
        let result = on_thread(move || {
            let tool = (!tool.is_empty()).then_some(tool.as_str());
            let old: Option<&BStr> = old_path.as_ref().map(|old| old.as_ref());
            let cancel = CancelToken::new();
            let git = tools::tools_git(&git)?;
            tools::run_difftool(&git, &cwd, &target, path.as_ref(), old, tool, &cancel)
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Err(err)) = result.recv().await {
                this.update_in(cx, |_, _, cx| {
                    let title = t!("external.diff_tool_failed").into_owned();
                    let message = Toast::error(tool_message(&err)).title(title);
                    note(message, cx)
                })
                .ok();
            }
        })
        .detach();
    }

    /// `git mergetool` for one conflicted file; git marks it resolved when the tool reports
    /// success. The status is read again once the tool is closed. A tool that runs in a
    /// terminal (vimdiff) is started in the user's terminal instead, and the watcher sees
    /// the file resolved when it's done there.
    pub(crate) fn run_merge_tool(
        &mut self,
        path: BString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (RepoState::Loaded(repo), Some(cwd)) = (&self.state, self.repository_folder()) else {
            return;
        };
        let git = repo.git.clone();
        let settings = SettingsStore::settings(cx).tools.clone();
        let tool = settings.merge_tool.trim().to_owned();
        // Ok(Some(tool)): started in a terminal; Ok(None): the tool ran and was closed.
        let result = on_thread(move || -> Result<Option<String>, String> {
            let cancel = CancelToken::new();
            let tool = (!tool.is_empty()).then_some(tool.as_str());
            let git = tools::tools_git(&git).map_err(|e| tool_message(&e))?;
            let tool =
                tools::merge_tool(&git, &cwd, tool, &cancel).map_err(|e| tool_message(&e))?;
            if !tools::is_terminal_tool(&tool) {
                tools::run_merge_tool_as(&git, &cwd, path.as_ref(), &tool, &cancel)
                    .map_err(|e| tool_message(&e))?;
                return Ok(None);
            }
            let backup_set =
                tools::keep_backup_set(&git, &cwd, &cancel).map_err(|e| e.summary())?;
            let mut command = vec![std::ffi::OsString::from("git")];
            command.extend(tools::mergetool_args(&tool, path.as_ref(), backup_set));
            let env = Environment::current();
            programs::terminal_running(&settings.terminal, &cwd, &command, &env)
                .and_then(|launch| programs::launch(&launch))
                .map_err(|e| launch_message(&e))?;
            Ok(Some(tool))
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = result.recv().await;
            this.update_in(cx, |this, _, cx| {
                match result {
                    Ok(Err(err)) => {
                        let title = t!("external.merge_tool_failed").into_owned();
                        note(Toast::error(err).title(title), cx);
                    }
                    Ok(Ok(Some(tool))) => {
                        let text = t!("external.merge_tool_in_terminal", tool = tool);
                        note(Toast::info(text.into_owned()), cx);
                    }
                    _ => {}
                }
                this.refresh_status(cx);
            })
            .ok();
        })
        .detach();
    }
}
