//! The user's external diff and merge tools (meld, kdiff3, Beyond Compare, …), started
//! through git (`git difftool`, `git mergetool`) so git's own tool configuration applies:
//! `difftool.<tool>.cmd`/`.path`, `mergetool.<tool>.trustExitCode`, `mergetool.keepBackup`.
//!
//! Both calls block until the tool is closed; run them on a worker thread. Paths are
//! relative to the work tree's top, and `cwd` must be that top directory.
//!
//! The tool is resolved here (the same keys, in the same order, as git's
//! `git-mergetool--lib`) and always passed to git as `--tool=<name>`: git then never falls
//! back to guessing a tool (it does when a configured name is unknown), and a tool name
//! that doesn't exist is an error instead.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;

use gix::ObjectId;
use gix::bstr::BStr;
use gix::hash::Kind;

use crate::cancel::CancelToken;
use crate::cli::{GitCli, GitError};
use crate::read::object_format;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    Diff,
    Merge,
}

/// What a diff tool compares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffToolTarget {
    /// `base` (HEAD's commit, or `None` when HEAD is unborn: the empty tree) against the
    /// working-tree file.
    WorkingTree { base: Option<ObjectId> },
    /// A commit against its first parent (`None` for a root commit: the empty tree).
    Commit {
        parent: Option<ObjectId>,
        commit: ObjectId,
    },
}

#[derive(Debug)]
pub enum ToolError {
    /// No tool given and none configured in git (`diff.guitool`/`diff.tool`, or
    /// `merge.guitool`/`merge.tool`).
    NotConfigured(ToolKind),
    /// In a Flatpak, where the tools run on the host with its git: there is none there.
    NoHostGit,
    /// git knows the merge tool but can't find its program (vimdiff without `vim`). Git
    /// would write the merge's temporary files and then give up, and in a terminal the
    /// window would close before anyone saw why.
    MergeToolMissing(String),
    Git(GitError),
}

/// The git that starts the user's diff and merge tools. In a Flatpak the tools are the
/// host's programs, outside the sandbox, so git runs there too (`flatpak-spawn --host`),
/// with the user's own tool setup and temporary files the tools can read; elsewhere it's
/// `git` itself.
pub fn tools_git(git: &GitCli) -> Result<GitCli, ToolError> {
    // Flatpak puts this file at the root of every sandbox.
    tools_git_with(git, Path::new("/.flatpak-info"), Path::new("flatpak-spawn"))
}

/// [`tools_git`] with the Flatpak marker file and the spawner given (tests, #50).
pub fn tools_git_with(
    git: &GitCli,
    flatpak_info: &Path,
    spawn: &Path,
) -> Result<GitCli, ToolError> {
    if !flatpak_info.exists() {
        return Ok(git.clone());
    }
    git.on_host(spawn).map_err(|_| ToolError::NoHostGit)
}

impl fmt::Display for ToolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotConfigured(ToolKind::Diff) => f.write_str(
                "No diff tool is set up: set diff.tool in your git config \
                 (for example `git config --global diff.tool meld`).",
            ),
            Self::NotConfigured(ToolKind::Merge) => f.write_str(
                "No merge tool is set up: set merge.tool in your git config \
                 (for example `git config --global merge.tool meld`).",
            ),
            Self::NoHostGit => f.write_str(
                "Diff and merge tools run on the system outside the Flatpak sandbox, with its \
                 git, and git wasn't found there.",
            ),
            Self::MergeToolMissing(tool) => write!(
                f,
                "The merge tool {tool} isn't installed: git can't find its program. Install \
                 it, or set mergetool.{tool}.path in your git config."
            ),
            Self::Git(err) => err.fmt(f),
        }
    }
}

impl std::error::Error for ToolError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NotConfigured(_) | Self::NoHostGit | Self::MergeToolMissing(_) => None,
            Self::Git(err) => Some(err),
        }
    }
}

impl From<GitError> for ToolError {
    fn from(err: GitError) -> Self {
        Self::Git(err)
    }
}

/// The config keys git reads for `kind`, in its order (`get_configured_merge_tool` in
/// `git-mergetool--lib`, GUI mode): a diff tool falls back to the merge tool.
fn tool_keys(kind: ToolKind) -> &'static [&'static str] {
    match kind {
        ToolKind::Diff => &["diff.guitool", "merge.guitool", "diff.tool", "merge.tool"],
        ToolKind::Merge => &["merge.guitool", "merge.tool"],
    }
}

/// The tool git would start for `kind`: the GUI tool (`diff.guitool`, or `merge.guitool`)
/// when set, else the plain one (`diff.tool`, `merge.tool`); `None` when none is set.
/// Like git, a diff tool falls back to the merge tool: with only `merge.tool = meld`,
/// `git difftool` starts meld too. Empty values count as unset.
pub fn configured_tool(
    git: &GitCli,
    cwd: &Path,
    kind: ToolKind,
    cancel: &CancelToken,
) -> Result<Option<String>, GitError> {
    // One process for all keys: `key\nvalue\0` records, keys lowercased by git. Exit
    // code 1 means none is set.
    let output = git
        .command(cwd)
        .args([
            "config",
            "-z",
            "--get-regexp",
            r"^(diff|merge)\.(gui)?tool$",
        ])
        .read_only()
        .allow_exit_codes(&[1])
        .run(cancel)?;
    let mut values: Vec<(&[u8], &[u8])> = Vec::new();
    for record in output.stdout.split(|&b| b == 0) {
        let mut parts = record.splitn(2, |&b| b == b'\n');
        let (Some(key), Some(value)) = (parts.next(), parts.next()) else {
            continue;
        };
        // A later value overrides an earlier one (a repository's over the global one).
        values.retain(|(k, _)| *k != key);
        values.push((key, value));
    }
    Ok(tool_keys(kind).iter().find_map(|wanted| {
        values
            .iter()
            .find(|(key, value)| key.eq_ignore_ascii_case(wanted.as_bytes()) && !value.is_empty())
            .map(|(_, value)| String::from_utf8_lossy(value).into_owned())
    }))
}

/// `tool`, or the configured one for `kind`.
fn resolve_tool(
    git: &GitCli,
    cwd: &Path,
    kind: ToolKind,
    tool: Option<&str>,
    cancel: &CancelToken,
) -> Result<String, ToolError> {
    match tool.filter(|t| !t.is_empty()) {
        Some(tool) => Ok(tool.to_owned()),
        None => configured_tool(git, cwd, kind, cancel)?.ok_or(ToolError::NotConfigured(kind)),
    }
}

/// Merge tools that run inside a terminal (git's `vimdiff` and `nvimdiff`, in all their
/// layouts: `vimdiff1`…): started without one, they fail at once. (`gvimdiff` and the rest
/// open windows of their own.)
pub fn is_terminal_tool(name: &str) -> bool {
    let base = name.trim_end_matches(|c: char| c.is_ascii_digit());
    matches!(base, "vimdiff" | "nvimdiff")
}

/// The merge tool [`run_mergetool`] starts: `tool`, or the configured one. Fails with
/// [`ToolError::MergeToolMissing`] when git can't find the tool's program (#84).
pub fn merge_tool(
    git: &GitCli,
    cwd: &Path,
    tool: Option<&str>,
    cancel: &CancelToken,
) -> Result<String, ToolError> {
    let tool = resolve_tool(git, cwd, ToolKind::Merge, tool, cancel)?;
    if merge_tool_missing(git, cwd, &tool, cancel)? {
        return Err(ToolError::MergeToolMissing(tool));
    }
    Ok(tool)
}

/// Whether `git mergetool --tool-help` lists `tool` as "valid, but not currently
/// available". That list looks for each tool's default program only, so a tool of the
/// user's own (`mergetool.<tool>.cmd`) or with its own program (`.path`) is taken as it is.
fn merge_tool_missing(
    git: &GitCli,
    cwd: &Path,
    tool: &str,
    cancel: &CancelToken,
) -> Result<bool, GitError> {
    let own = git
        .command(cwd)
        .args(["config", "--get-regexp"])
        .arg(format!(r"^mergetool\.{}\.(cmd|path)$", regex_escape(tool)))
        .allow_exit_codes(&[1])
        .read_only()
        .run(cancel)?;
    if own.status == 0 {
        return Ok(false);
    }
    let help = git
        .command(cwd)
        .args(["mergetool", "--tool-help"])
        .read_only()
        .run(cancel)?;
    Ok(unavailable_tools(&String::from_utf8_lossy(&help.stdout)).any(|name| name == tool))
}

/// The tools under "valid, but not currently available" in `--tool-help`'s text (each line
/// is the name, then a description).
fn unavailable_tools(help: &str) -> impl Iterator<Item = &str> {
    help.lines()
        .skip_while(|line| !line.contains("not currently available"))
        .skip(1)
        .take_while(|line| !line.trim().is_empty())
        .filter_map(|line| line.split_whitespace().next())
}

/// `text` with the characters an extended regular expression gives a meaning escaped.
fn regex_escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        if "\\.^$|?*+()[]{}".contains(c) {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}

/// Whether the user set `mergetool.keepBackup` (to anything) in git's config.
pub fn keep_backup_set(git: &GitCli, cwd: &Path, cancel: &CancelToken) -> Result<bool, GitError> {
    let output = git
        .command(cwd)
        .args(["config", "--get", "mergetool.keepBackup"])
        .allow_exit_codes(&[1])
        .read_only()
        .run(cancel)?;
    Ok(output.status == 0)
}

/// Git's arguments for merging `path` with `tool`: no prompt, literal pathspecs, and no
/// `<path>.orig` backup left behind unless the user asked for one (`backup_set`: see
/// [`keep_backup_set`]; git's own default keeps it, untracked).
pub fn mergetool_args(tool: &str, path: &BStr, backup_set: bool) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec!["--literal-pathspecs".into()];
    if !backup_set {
        args.extend(["-c".into(), "mergetool.keepBackup=false".into()]);
    }
    args.extend([
        "mergetool".into(),
        "--no-prompt".into(),
        format!("--tool={tool}").into(),
        "--".into(),
        os(path).to_owned(),
    ]);
    args
}

/// The locale the tool should show in. The runner forces `LC_MESSAGES=C` and removes
/// `LANGUAGE` so git's messages can be classified; a GUI tool gets the user's back:
/// `LC_MESSAGES` from the first set of `LC_ALL`, `LC_MESSAGES`, `LANG`, and `LANGUAGE`
/// as it was. `var` reads this process's environment.
fn tool_locale(var: impl Fn(&str) -> Option<OsString>) -> Vec<(&'static str, OsString)> {
    let var = |name: &str| var(name).filter(|v| !v.is_empty());
    let mut env = Vec::new();
    if let Some(messages) = ["LC_ALL", "LC_MESSAGES", "LANG"].into_iter().find_map(var) {
        env.push(("LC_MESSAGES", messages));
    }
    if let Some(language) = var("LANGUAGE") {
        env.push(("LANGUAGE", language));
    }
    env
}

fn os(path: &BStr) -> &OsStr {
    OsStr::from_bytes(path)
}

/// Runs `git difftool --no-prompt` for one file and waits until the tool is closed.
///
/// `tool`: a tool name overriding git's config; `None` uses the configured one (see
/// [`configured_tool`]) and fails with [`ToolError::NotConfigured`] when there is none,
/// before running anything. `old_path`: the file's previous name for a rename (both
/// paths are passed, with `-M`, so the tool shows the rename; git then names the pair by
/// the old path in `$MERGED`/`$BASE`, while `$LOCAL`/`$REMOTE` are temporary files ending
/// in each side's own name).
///
/// git ignores the tool's exit code unless `difftool.trustExitCode` is set; a tool that
/// can't be started (exit 126/127) or dies from a signal is always an error.
pub fn run_difftool(
    git: &GitCli,
    cwd: &Path,
    target: &DiffToolTarget,
    path: &BStr,
    old_path: Option<&BStr>,
    tool: Option<&str>,
    cancel: &CancelToken,
) -> Result<(), ToolError> {
    let tool = resolve_tool(git, cwd, ToolKind::Diff, tool, cancel)?;
    // A missing side (a root commit's parent, an unborn branch) is the empty tree of the
    // repository's hash, SHA-1 or SHA-256.
    let tree =
        |id: Option<ObjectId>, kind: Kind| id.unwrap_or_else(|| kind.empty_tree()).to_string();
    let mut command = git
        .command(cwd)
        .args(["difftool", "--no-prompt"])
        .arg(format!("--tool={tool}"));
    if old_path.is_some() {
        command = command.arg("-M");
    }
    command = match target {
        DiffToolTarget::WorkingTree { base } => {
            let kind = match base {
                Some(base) => base.kind(),
                None => object_format(git, cwd, cancel)?,
            };
            command.arg(tree(*base, kind))
        }
        DiffToolTarget::Commit { parent, commit } => command
            .arg(tree(*parent, commit.kind()))
            .arg(commit.to_string()),
    };
    command = command.arg("--").arg(os(path));
    if let Some(old_path) = old_path {
        command = command.arg(os(old_path));
    }
    for (key, value) in tool_locale(|name| std::env::var_os(name)) {
        command = command.env(key, value);
    }
    command
        .env("GIT_LITERAL_PATHSPECS", "1")
        .read_only()
        .run(cancel)?;
    Ok(())
}

/// Runs `git mergetool --no-prompt` for one conflicted file and waits until the tool is
/// closed; git marks it resolved (stages it) when the tool reports success. Same tool
/// resolution as [`run_difftool`], with `merge.guitool` / `merge.tool` and
/// [`ToolError::NotConfigured`]`(Merge)`.
///
/// "Success" is the tool's exit code with `mergetool.<tool>.trustExitCode`, otherwise
/// whether the file was written. A failed merge is an error and leaves the file as it was
/// (still conflicted). A resolved file's conflicted version is kept as `<path>.orig` only
/// when the user set `mergetool.keepBackup` ([`mergetool_args`]). A path that isn't conflicted
/// succeeds without starting anything. Conflicts git would ask about instead of starting
/// a tool (modify/delete, symlinks, submodules) fail: git's question reads stdin, which is
/// empty.
///
/// A git quirk: `git-mergetool` word-splits its list of conflicted files without
/// quoting, so the shell glob-expands each name. A conflicted file named `*.txt` makes it
/// also merge every other conflicted `*.txt` (and fail on matching files that aren't
/// conflicted). Names without `*`, `?` or `[` are unaffected.
pub fn run_mergetool(
    git: &GitCli,
    cwd: &Path,
    path: &BStr,
    tool: Option<&str>,
    cancel: &CancelToken,
) -> Result<(), ToolError> {
    let tool = merge_tool(git, cwd, tool, cancel)?;
    run_merge_tool_as(git, cwd, path, &tool, cancel)
}

/// [`run_mergetool`] with `tool` as [`merge_tool`] gave it (resolved and found).
pub fn run_merge_tool_as(
    git: &GitCli,
    cwd: &Path,
    path: &BStr,
    tool: &str,
    cancel: &CancelToken,
) -> Result<(), ToolError> {
    let backup_set = keep_backup_set(git, cwd, cancel)?;
    let mut command = git
        .command(cwd)
        .args(mergetool_args(tool, path, backup_set));
    for (key, value) in tool_locale(|name| std::env::var_os(name)) {
        command = command.env(key, value);
    }
    command.run(cancel)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn locale(vars: &[(&str, &str)]) -> Vec<(&'static str, String)> {
        tool_locale(|name| {
            vars.iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| OsString::from(v))
        })
        .into_iter()
        .map(|(k, v)| (k, v.into_string().unwrap()))
        .collect()
    }

    #[test]
    fn terminal_tools_are_vimdiff_and_nvimdiff_in_every_layout() {
        for name in ["vimdiff", "vimdiff1", "vimdiff3", "nvimdiff", "nvimdiff2"] {
            assert!(is_terminal_tool(name), "{name}");
        }
        for name in ["gvimdiff", "meld", "kdiff3", "emerge", "vimdiffx", ""] {
            assert!(!is_terminal_tool(name), "{name}");
        }
    }

    #[test]
    fn mergetool_arguments_leave_no_backup_unless_asked() {
        let text = |args: Vec<OsString>| -> Vec<String> {
            args.into_iter()
                .map(|arg| arg.into_string().unwrap())
                .collect()
        };
        let path = BStr::new("dir/a b.txt");
        assert_eq!(
            text(mergetool_args("vimdiff", path, false)),
            [
                "--literal-pathspecs",
                "-c",
                "mergetool.keepBackup=false",
                "mergetool",
                "--no-prompt",
                "--tool=vimdiff",
                "--",
                "dir/a b.txt"
            ]
        );
        let kept = text(mergetool_args("meld", path, true));
        assert!(
            !kept.iter().any(|arg| arg.contains("keepBackup")),
            "{kept:?}"
        );
    }

    #[test]
    fn the_tool_gets_the_users_locale() {
        assert_eq!(locale(&[]), []);
        assert_eq!(
            locale(&[("LANG", "de_DE.UTF-8")]),
            [("LC_MESSAGES", "de_DE.UTF-8".to_owned())]
        );
        assert_eq!(
            locale(&[("LANG", "de_DE.UTF-8"), ("LC_MESSAGES", "fr_FR.UTF-8")]),
            [("LC_MESSAGES", "fr_FR.UTF-8".to_owned())]
        );
        assert_eq!(
            locale(&[
                ("LANG", "de_DE.UTF-8"),
                ("LC_MESSAGES", ""),
                ("LC_ALL", "ru_RU.UTF-8"),
                ("LANGUAGE", "ru:en"),
            ]),
            [
                ("LC_MESSAGES", "ru_RU.UTF-8".to_owned()),
                ("LANGUAGE", "ru:en".to_owned())
            ]
        );
        assert_eq!(
            locale(&[("LC_ALL", ""), ("LANGUAGE", "uk")]),
            [("LANGUAGE", "uk".to_owned())]
        );
    }

    #[test]
    fn messages_name_the_setting() {
        let diff = ToolError::NotConfigured(ToolKind::Diff).to_string();
        assert!(diff.contains("diff.tool"), "{diff}");
        let merge = ToolError::NotConfigured(ToolKind::Merge).to_string();
        assert!(merge.contains("merge.tool"), "{merge}");
    }
}
