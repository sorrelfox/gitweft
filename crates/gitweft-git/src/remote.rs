//! Network operations: fetch, pull and push. They always run the user's `git`, so
//! credential helpers, `~/.ssh/config`, proxies and hooks behave as in a terminal.
//! `--progress` makes git report progress on stderr even without a terminal; cancelling
//! kills the whole process group, ssh included.

use std::collections::HashMap;
use std::path::Path;

use crate::cancel::CancelToken;
use crate::cli::{GitCli, GitError, GitOutput, StderrEvent};
use crate::read::Tracking;

/// Names of the configured remotes, in git's order.
pub fn remote_names(
    git: &GitCli,
    workdir: &Path,
    cancel: &CancelToken,
) -> Result<Vec<String>, GitError> {
    let output = git.command(workdir).arg("remote").read_only().run(cancel)?;
    Ok(output.stdout_str().lines().map(str::to_owned).collect())
}

/// A configured remote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteInfo {
    pub name: String,
    /// Where it fetches from (`remote.<name>.url`); empty if none is set.
    pub url: String,
    /// Where it pushes to, when that differs (`remote.<name>.pushurl`).
    pub push_url: Option<String>,
}

/// The configured remotes with their URLs, in git's order (`git remote` sorts by name).
///
/// The URLs come from the configuration, not from `git remote -v`: that prints them after
/// `url.<base>.insteadOf` rewriting (so not what `set-url` edits), lists a second `url`
/// only as a push line, and can't be split reliably when a URL itself ends in
/// ` (fetch)`. Config keys split cleanly at the *last* dot even though remote names may
/// contain dots, and `-z` delimits values, so URLs may hold spaces.
pub fn remotes(
    git: &GitCli,
    workdir: &Path,
    cancel: &CancelToken,
) -> Result<Vec<RemoteInfo>, GitError> {
    let names = remote_names(git, workdir, cancel)?;
    let config = git
        .command(workdir)
        .args([
            "config",
            "-z",
            "--get-regexp",
            r"^remote\..*\.(url|pushurl)$",
        ])
        // Exit code 1: nothing matched, i.e. no remote has a URL.
        .allow_exit_codes(&[1])
        .read_only()
        .run(cancel)?;
    Ok(parse_remotes(names, &config.stdout_str()))
}

/// Pairs `names` with their URLs from `git config -z --get-regexp` output
/// (`remote.<name>.url\n<value>\0…`).
fn parse_remotes(names: Vec<String>, config: &str) -> Vec<RemoteInfo> {
    let mut urls: HashMap<&str, (Option<&str>, Option<&str>)> = HashMap::new();
    for record in config.split('\0') {
        // A key without a value (a bare `url` line in the file) has no newline.
        let (key, value) = record.split_once('\n').unwrap_or((record, ""));
        let Some((name, var)) = key
            .strip_prefix("remote.")
            .and_then(|key| key.rsplit_once('.'))
        else {
            continue;
        };
        let entry = urls.entry(name).or_default();
        let slot = match var {
            "url" => &mut entry.0,
            "pushurl" => &mut entry.1,
            _ => continue,
        };
        // With several, git fetches from the first `url` (and pushes to all of them).
        slot.get_or_insert(value);
    }
    names
        .into_iter()
        .map(|name| {
            let (url, push_url) = urls.get(name.as_str()).copied().unwrap_or_default();
            let url = url.unwrap_or_default().to_owned();
            let push_url = push_url.filter(|push| *push != url).map(str::to_owned);
            RemoteInfo {
                name,
                url,
                push_url,
            }
        })
        .collect()
}

/// Whether `name` can be a remote's name. Git's own rule is that
/// `refs/remotes/<name>/<branch>` must be a valid ref; we also reject a leading `-`, which
/// git accepts after `--` but which other commands (`push`, `fetch`) would read as an option.
pub fn is_valid_remote_name(
    git: &GitCli,
    workdir: &Path,
    name: &str,
    cancel: &CancelToken,
) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && git
            .command(workdir)
            .arg("check-ref-format")
            .arg(format!("refs/remotes/{name}/main"))
            .read_only()
            .run(cancel)
            .is_ok()
}

/// Adds a remote without fetching it. Git refuses an existing or invalid name.
pub fn add_remote(
    git: &GitCli,
    workdir: &Path,
    name: &str,
    url: &str,
    cancel: &CancelToken,
) -> Result<(), GitError> {
    git.command(workdir)
        .args(["remote", "add", "--", name, url])
        .run(cancel)?;
    Ok(())
}

/// Renames a remote: its remote-tracking refs move to `refs/remotes/<new>/…`, and
/// branches whose upstream was on it follow.
pub fn rename_remote(
    git: &GitCli,
    workdir: &Path,
    old: &str,
    new: &str,
    cancel: &CancelToken,
) -> Result<(), GitError> {
    git.command(workdir)
        .args(["remote", "rename", "--", old, new])
        .run(cancel)?;
    Ok(())
}

/// Removes a remote with its remote-tracking refs; branches that tracked it lose their
/// upstream.
pub fn remove_remote(
    git: &GitCli,
    workdir: &Path,
    name: &str,
    cancel: &CancelToken,
) -> Result<(), GitError> {
    git.command(workdir)
        .args(["remote", "remove", "--", name])
        .run(cancel)?;
    Ok(())
}

/// Changes where a remote fetches from (and pushes to, unless it has a separate push URL).
pub fn set_remote_url(
    git: &GitCli,
    workdir: &Path,
    name: &str,
    url: &str,
    cancel: &CancelToken,
) -> Result<(), GitError> {
    git.command(workdir)
        .args(["remote", "set-url", "--", name, url])
        .run(cancel)?;
    Ok(())
}

/// Sets where a remote pushes to (`git remote set-url --push`: `remote.<name>.pushurl`),
/// or with `None` drops its separate push URL, so pushes go to its URL again. `git
/// remote` can't drop one, so that's `git config --unset-all` of every `pushurl` of the
/// remote; a remote without one is no error.
pub fn set_remote_push_url(
    git: &GitCli,
    workdir: &Path,
    name: &str,
    url: Option<&str>,
    cancel: &CancelToken,
) -> Result<(), GitError> {
    match url {
        Some(url) => {
            git.command(workdir)
                .args(["remote", "set-url", "--push", "--", name, url])
                .run(cancel)?;
        }
        None => {
            git.command(workdir)
                .args(["config", "--unset-all"])
                .arg(format!("remote.{name}.pushurl"))
                // 5: there was no such key.
                .allow_exit_codes(&[5])
                .run(cancel)?;
        }
    }
    Ok(())
}

/// Which remotes to fetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchTarget<'a> {
    /// Every configured remote (`--all`).
    All,
    Remote(&'a str),
}

/// Fetches; with `prune`, remote-tracking branches deleted on the remote go too.
pub fn fetch(
    git: &GitCli,
    workdir: &Path,
    target: FetchTarget<'_>,
    prune: bool,
    cancel: &CancelToken,
    on_event: impl FnMut(StderrEvent),
) -> Result<GitOutput, GitError> {
    let mut command = git.command(workdir).args(["fetch", "--progress"]);
    if prune {
        command = command.arg("--prune");
    }
    command = match target {
        FetchTarget::All => command.arg("--all"),
        FetchTarget::Remote(remote) => command.arg(remote),
    };
    command.run_with_events(cancel, on_event)
}

/// The periodic fetch the user can opt into: every remote, pruned, and never a prompt
/// ([`GitCommand::non_interactive`](crate::cli::GitCommand::non_interactive)), so a remote
/// that needs a password fails quietly instead of opening a dialog out of nowhere.
/// `--quiet` leaves only errors on stderr (nobody watches its progress), so a failure's
/// summary is git's error line rather than the last progress record.
pub fn fetch_background(
    git: &GitCli,
    workdir: &Path,
    cancel: &CancelToken,
) -> Result<GitOutput, GitError> {
    git.command(workdir)
        .args(["fetch", "--all", "--prune", "--quiet"])
        .non_interactive()
        .run(cancel)
}

/// How much history a shallow clone should gain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deepen {
    /// This many more commits below the current boundary (`--deepen=<n>`).
    By(u32),
    /// The whole history (`--unshallow`); git refuses this on a complete repository.
    All,
}

/// Deepens a shallow clone from the default remote: the current branch's upstream
/// remote, else `origin` (else, with a single remote, that one). Git deepens every
/// branch it fetches, not just the current one.
pub fn deepen(
    git: &GitCli,
    workdir: &Path,
    how: Deepen,
    cancel: &CancelToken,
    on_event: impl FnMut(StderrEvent),
) -> Result<GitOutput, GitError> {
    let command = git.command(workdir).args(["fetch", "--progress"]);
    let command = match how {
        Deepen::By(commits) => command.arg(format!("--deepen={}", commits.max(1))),
        Deepen::All => command.arg("--unshallow"),
    };
    command.run_with_events(cancel, on_event)
}

/// How `pull` combines the upstream branch with the local one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PullMode {
    /// Whatever `pull.rebase` / `pull.ff` say. Without them git refuses to combine
    /// divergent branches ([`crate::GitErrorKind::DivergentBranches`]).
    #[default]
    Configured,
    Merge,
    Rebase,
    FastForwardOnly,
}

/// Pulls the current branch's upstream; `autostash` shelves local changes around it.
pub fn pull(
    git: &GitCli,
    workdir: &Path,
    mode: PullMode,
    autostash: bool,
    cancel: &CancelToken,
    on_event: impl FnMut(StderrEvent),
) -> Result<GitOutput, GitError> {
    let mut command = git.command(workdir).args(["pull", "--progress"]);
    command = match mode {
        PullMode::Configured => command,
        PullMode::Merge => command.arg("--no-rebase"),
        PullMode::Rebase => command.arg("--rebase"),
        PullMode::FastForwardOnly => command.arg("--ff-only"),
    };
    if autostash {
        command = command.arg("--autostash");
    }
    command.run_with_events(cancel, on_event)
}

/// How hard to push.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ForceMode {
    #[default]
    None,
    /// `--force-with-lease --force-if-includes`: replace the remote branch only if it
    /// still is what we last fetched *and* that state is part of the local branch's
    /// history (as after an amend or rebase), so nobody else's unseen work is lost.
    WithLease,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushRequest {
    pub remote: String,
    /// The local branch to push.
    pub branch: String,
    /// Its name on the remote (usually the same).
    pub remote_branch: String,
    /// Make the pushed branch the local branch's upstream.
    pub set_upstream: bool,
    pub force: ForceMode,
}

/// Where `branch` goes when pushed: to its upstream if it has one; otherwise to a branch
/// of the same name on `origin` (or on the only remote), which then becomes its upstream.
/// `None` when that's ambiguous (several remotes, none called `origin`) or there's none.
///
/// The upstream is taken as configured (`branch.<name>.remote` and `.merge`), so a branch
/// named differently there, or a fetch refspec that maps names, pushes to the right one.
/// A local upstream (`remote = .`) isn't a remote: the branch goes to a branch of its
/// own name as when it has none, but that doesn't replace the upstream.
pub fn push_target(branch: &str, tracking: &[Tracking], remotes: &[String]) -> Option<PushRequest> {
    let entry = tracking.iter().find(|t| t.branch == branch);
    let local_upstream = entry.is_some_and(|t| t.upstream_remote.as_deref() == Some("."));
    if let Some(entry) = entry.filter(|_| !local_upstream)
        && let (Some(remote), Some(remote_branch)) =
            (&entry.upstream_remote, &entry.upstream_branch)
        && remotes.contains(remote)
    {
        return Some(PushRequest {
            remote: remote.clone(),
            branch: branch.to_owned(),
            remote_branch: remote_branch.clone(),
            set_upstream: false,
            force: ForceMode::None,
        });
    }
    // Without the configuration (an older reading): the tracking ref's name.
    let upstream = entry
        .filter(|_| !local_upstream)
        .and_then(|t| t.upstream.as_deref());
    if let Some(upstream) = upstream {
        // Remote names may contain slashes too: take the longest one that matches.
        let remote = remotes
            .iter()
            .filter(|remote| {
                upstream
                    .strip_prefix(remote.as_str())
                    .is_some_and(|rest| rest.starts_with('/'))
            })
            .max_by_key(|remote| remote.len());
        if let Some(remote) = remote {
            return Some(PushRequest {
                remote: remote.clone(),
                branch: branch.to_owned(),
                remote_branch: upstream[remote.len() + 1..].to_owned(),
                set_upstream: false,
                force: ForceMode::None,
            });
        }
    }
    let remote = match remotes {
        [only] => only,
        _ => remotes.iter().find(|remote| *remote == "origin")?,
    };
    Some(PushRequest {
        remote: remote.clone(),
        branch: branch.to_owned(),
        remote_branch: branch.to_owned(),
        // A local upstream stays the upstream.
        set_upstream: !local_upstream,
        force: ForceMode::None,
    })
}

/// Pushes one branch. (`--force-if-includes` needs git 2.30; the minimum is 2.39.)
pub fn push(
    git: &GitCli,
    workdir: &Path,
    request: &PushRequest,
    cancel: &CancelToken,
    on_event: impl FnMut(StderrEvent),
) -> Result<GitOutput, GitError> {
    let mut command = git.command(workdir).args(["push", "--progress"]);
    if request.set_upstream {
        command = command.arg("--set-upstream");
    }
    if request.force == ForceMode::WithLease {
        command = command.args(["--force-with-lease", "--force-if-includes"]);
    }
    // The branch names as bytes: they may be encoded (`undo::ref_name_text`) for a name
    // that isn't UTF-8; any other text is taken as it is.
    use std::os::unix::ffi::OsStringExt as _;
    let mut refspec = b"refs/heads/".to_vec();
    refspec.extend(crate::undo::ref_name_bytes(&request.branch));
    refspec.extend_from_slice(b":refs/heads/");
    refspec.extend(crate::undo::ref_name_bytes(&request.remote_branch));
    command
        .arg(&request.remote)
        .arg(std::ffi::OsString::from_vec(refspec))
        .run_with_events(cancel, on_event)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracked(branch: &str, upstream: Option<&str>) -> Tracking {
        Tracking {
            branch: branch.into(),
            upstream: upstream.map(Into::into),
            ..Tracking::default()
        }
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn remote_urls_come_from_config_keys_split_at_the_last_dot() {
        let config = "remote.my.remote.url\nfile:///a b (fetch)\0\
                      remote.my.remote.pushurl\nssh://host/a\0\
                      remote.corp/mirror.url\nfile:///first\0\
                      remote.corp/mirror.url\nfile:///second\0\
                      remote.same.url\nfile:///s\0\
                      remote.same.pushurl\nfile:///s\0\
                      remote.flag.url\0";
        let remotes = parse_remotes(
            names(&["corp/mirror", "flag", "missing", "my.remote", "same"]),
            config,
        );
        let got: Vec<_> = remotes
            .iter()
            .map(|r| (r.name.as_str(), r.url.as_str(), r.push_url.as_deref()))
            .collect();
        assert_eq!(
            got,
            [
                ("corp/mirror", "file:///first", None),
                ("flag", "", None),
                ("missing", "", None),
                ("my.remote", "file:///a b (fetch)", Some("ssh://host/a")),
                ("same", "file:///s", None),
            ]
        );
    }

    #[test]
    fn push_goes_to_the_upstream_even_under_another_name() {
        let tracking = [tracked("fix", Some("upstream/team/fix-123"))];
        let request = push_target("fix", &tracking, &names(&["origin", "upstream"])).unwrap();
        assert_eq!(
            (request.remote.as_str(), request.remote_branch.as_str()),
            ("upstream", "team/fix-123")
        );
        assert!(!request.set_upstream);
    }

    #[test]
    fn remote_names_with_slashes_pick_the_longest_match() {
        let tracking = [tracked("main", Some("corp/mirror/main"))];
        let request = push_target("main", &tracking, &names(&["corp", "corp/mirror"])).unwrap();
        assert_eq!(
            (request.remote.as_str(), request.remote_branch.as_str()),
            ("corp/mirror", "main")
        );
    }

    /// A branch as `branch_tracking` reads it: with its configured remote and merge ref.
    fn configured(branch: &str, upstream: &str, remote: &str, merge: &str) -> Tracking {
        Tracking {
            upstream_remote: Some(remote.into()),
            upstream_branch: Some(merge.into()),
            ..tracked(branch, Some(upstream))
        }
    }

    #[test]
    fn push_follows_the_configured_merge_ref_not_the_tracking_name() {
        // A fetch refspec maps origin's branches under `mirror/`: the tracking ref's name
        // says nothing about the branch on the remote.
        let tracking = [configured("fix", "mirror/fix", "origin", "team/fix")];
        let request = push_target("fix", &tracking, &names(&["origin"])).unwrap();
        assert_eq!(
            (request.remote.as_str(), request.remote_branch.as_str()),
            ("origin", "team/fix")
        );
        assert!(!request.set_upstream);
    }

    #[test]
    fn a_local_upstream_is_kept() {
        // `branch.feature.remote = .`: the upstream is the local main, not a remote branch.
        let tracking = [configured("feature", "main", ".", "main")];
        let request = push_target("feature", &tracking, &names(&["origin"])).unwrap();
        assert_eq!(
            (request.remote.as_str(), request.remote_branch.as_str()),
            ("origin", "feature")
        );
        assert!(
            !request.set_upstream,
            "the local upstream must not be replaced"
        );
    }

    #[test]
    fn a_new_branch_goes_to_origin_and_gets_an_upstream() {
        let tracking = [tracked("feature", None)];
        let request = push_target("feature", &tracking, &names(&["fork", "origin"])).unwrap();
        assert_eq!(request.remote, "origin");
        assert_eq!(request.remote_branch, "feature");
        assert!(request.set_upstream);
        // The only remote is fine too, whatever its name.
        assert_eq!(
            push_target("feature", &tracking, &names(&["fork"]))
                .unwrap()
                .remote,
            "fork"
        );
        // Ambiguous, or nowhere to push.
        assert!(push_target("feature", &tracking, &names(&["a", "b"])).is_none());
        assert!(push_target("feature", &tracking, &[]).is_none());
    }
}
