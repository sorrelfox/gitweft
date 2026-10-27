//! Creating repositories: `git init` and `git clone`.

use std::path::Path;

use crate::cancel::CancelToken;
use crate::cli::{GitCli, GitError, GitErrorKind, GitOutput, StderrEvent};

/// Creates an empty repository in `dir` (made if missing; an existing repository there is
/// left as it is, like `git init` does).
pub fn init_repository(git: &GitCli, dir: &Path, cancel: &CancelToken) -> Result<(), GitError> {
    git.command(existing_ancestor(dir))
        .args(["init", "--quiet", "--"])
        .arg(dir)
        .run(cancel)?;
    Ok(())
}

/// Clones `url` into `dir`, which must not exist or be empty. git's `--` keeps a URL
/// starting with `-` from being read as an option.
pub fn clone_repository(
    git: &GitCli,
    url: &str,
    dir: &Path,
    recurse_submodules: bool,
    cancel: &CancelToken,
    on_event: impl FnMut(StderrEvent),
) -> Result<GitOutput, GitError> {
    let url = url.trim();
    if url.is_empty() {
        return Err(GitError::new(
            GitErrorKind::Other,
            "git clone",
            "fatal: no repository address given",
        ));
    }
    let mut command = git
        .command(existing_ancestor(dir))
        .args(["clone", "--progress"]);
    if recurse_submodules {
        command = command.arg("--recurse-submodules");
    }
    command
        .arg("--")
        .arg(url)
        .arg(dir)
        .run_with_events(cancel, on_event)
}

/// The directory name git would pick for a clone of `url`: its last path component
/// without `.git` (`https://host/team/app.git` and `git@host:team/app` give `app`).
pub fn clone_dir_name(url: &str) -> Option<String> {
    let mut rest = url.trim().trim_end_matches('/');
    for suffix in ["/.git", ".git"] {
        rest = rest.strip_suffix(suffix).unwrap_or(rest);
    }
    let rest = rest.trim_end_matches('/');
    // The part after the scheme or host: `host:path` (scp-like) or `scheme://host/path`.
    let name = rest.rsplit(['/', ':']).next()?.trim();
    (!name.is_empty() && name != "." && name != "..").then(|| name.to_owned())
}

/// `dir` itself, or its nearest ancestor that exists (git needs an existing working
/// directory to start in).
fn existing_ancestor(dir: &Path) -> &Path {
    dir.ancestors()
        .find(|path| path.is_dir())
        .unwrap_or(Path::new("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clone_dir_names_follow_git() {
        let cases = [
            ("https://github.com/team/app.git", Some("app")),
            ("https://github.com/team/app", Some("app")),
            ("https://github.com/team/app/", Some("app")),
            ("git@github.com:team/app.git", Some("app")),
            ("host:app", Some("app")),
            ("ssh://git@host:2222/srv/app.git", Some("app")),
            ("/srv/git/project/.git", Some("project")),
            ("file:///tmp/work", Some("work")),
            ("https://example.com/", Some("example.com")),
            ("  https://host/x.git  ", Some("x")),
            ("", None),
            ("host:", None),
            ("..", None),
        ];
        for (url, expected) in cases {
            assert_eq!(clone_dir_name(url).as_deref(), expected, "{url}");
        }
    }
}
