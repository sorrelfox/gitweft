//! Submodules: list, add, update (init + checkout), sync, deinit and remove, through the
//! git CLI.
//!
//! Paths are relative to the superproject root (`workdir` must be that root), kept as
//! `PathBuf` and read from `-z` output as bytes, so non-UTF-8 names survive. Paths go
//! after `--` and are taken literally (`GIT_LITERAL_PATHSPECS`), so a submodule named
//! `st*r` or `-x` means exactly that folder.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use gitweft_platform::bytes::path_from_bytes;
use gix::ObjectId;

use crate::cancel::CancelToken;
use crate::cli::{GitCli, GitError, GitErrorKind, StderrEvent};

/// Where a submodule's checkout stands relative to what the superproject records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmoduleState {
    /// Checked out at the commit the superproject records.
    UpToDate,
    /// Checked out at another commit (`+` in `git submodule status`), or a repository
    /// with no commit yet.
    Changed,
    /// Not initialised / not cloned (`-`): no repository in its folder.
    Uninitialized,
    /// A merge conflict on the submodule entry (`U`).
    Conflict,
}

/// How deep [`list_submodules_nested`] descends: submodules of submodules of … at most this
/// many levels below the top repository's own.
pub const MAX_NESTING: usize = 8;

/// A submodule of the superproject.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmoduleInfo {
    /// Path relative to the top repository's root (`mid/inner` for a nested one).
    pub path: PathBuf,
    /// The folder of the repository whose submodule this is, relative to the top
    /// repository's root: empty for the top repository's own submodules, `mid` for
    /// `mid/inner`.
    pub superproject: PathBuf,
    /// How deep it nests: 0 for the top repository's own submodules.
    pub depth: usize,
    /// Its name in `.gitmodules` (often equal to the path). The path itself when
    /// `.gitmodules` has no entry for it.
    pub name: String,
    /// The URL from `.gitmodules` (what `sync` copies into the config); `None` if missing.
    pub url: Option<String>,
    /// The commit the superproject records (index), if any. `None` for a conflict, like
    /// `git submodule status`.
    pub recorded: Option<ObjectId>,
    /// The commit checked out in the submodule, when it is initialised.
    pub checked_out: Option<ObjectId>,
    pub state: SubmoduleState,
}

/// The superproject's submodules (not recursive), in path order.
///
/// Built from three sources rather than by parsing `git submodule status`, whose output
/// is line-based, C-quotes unusual paths (and exits non-zero on a gitlink missing from
/// `.gitmodules`):
///
/// - `git ls-files --stage -z`: every gitlink (mode `160000`) with its recorded id, as
///   NUL-separated bytes. Several stages for one path mean a conflict.
/// - `git config -z --file .gitmodules --get-regexp`: names, paths and URLs. A name may
///   contain dots, so the last dot of the key separates the variable.
/// - For each gitlink whose folder has a `.git` (file or directory),
///   `git rev-parse --verify HEAD` run inside it. Without that check, git would discover
///   the superproject from an empty folder; with it, a folder that isn't a usable
///   repository (a broken `.git` file) is `Uninitialized`.
pub fn list_submodules(
    git: &GitCli,
    workdir: &Path,
    cancel: &CancelToken,
) -> Result<Vec<SubmoduleInfo>, GitError> {
    let index = git
        .command(workdir)
        .args(["ls-files", "--stage", "-z"])
        .read_only()
        .run(cancel)?;
    let gitlinks = parse_gitlinks(&index.stdout);
    if gitlinks.is_empty() {
        return Ok(Vec::new());
    }

    let modules = if workdir.join(".gitmodules").exists() {
        let output = git
            .command(workdir)
            .args([
                "config",
                "-z",
                "--file",
                ".gitmodules",
                "--get-regexp",
                r"^submodule\..*\.(path|url)$",
            ])
            .read_only()
            // 1: no matching key.
            .allow_exit_codes(&[1])
            .run(cancel)?;
        parse_gitmodules(&output.stdout)
    } else {
        Vec::new()
    };

    let mut submodules = Vec::with_capacity(gitlinks.len());
    for link in gitlinks {
        let module = modules
            .iter()
            .find(|m| m.path.as_deref() == Some(&*link.path));
        let checked_out = if link.conflict {
            Checkout::Missing
        } else {
            head_of(git, &workdir.join(&link.path), cancel)?
        };
        let state = match (&checked_out, link.conflict) {
            (_, true) => SubmoduleState::Conflict,
            (Checkout::Missing, _) => SubmoduleState::Uninitialized,
            (Checkout::At(id), _) if Some(*id) == link.recorded => SubmoduleState::UpToDate,
            _ => SubmoduleState::Changed,
        };
        submodules.push(SubmoduleInfo {
            name: module.map_or_else(
                || link.path.to_string_lossy().into_owned(),
                |m| m.name.clone(),
            ),
            url: module.and_then(|m| m.url.clone()),
            recorded: link.recorded,
            checked_out: match checked_out {
                Checkout::At(id) => Some(id),
                _ => None,
            },
            path: link.path,
            superproject: PathBuf::new(),
            depth: 0,
            state,
        });
    }
    Ok(submodules)
}

/// The top repository's submodules, each followed at once by its own submodules (and
/// theirs, pre-order), with paths relative to the top repository's root.
///
/// Only submodules that are set up (checked out: [`SubmoduleState::UpToDate`] or
/// [`SubmoduleState::Changed`] with a commit) are looked into; an uninitialised one has no
/// repository to list, a conflicted one no single checkout. At most [`MAX_NESTING`] levels
/// below the top are read. A nested repository that can't be listed just contributes no
/// children; an error listing the top repository, or a cancellation, is returned.
pub fn list_submodules_nested(
    git: &GitCli,
    workdir: &Path,
    cancel: &CancelToken,
) -> Result<Vec<SubmoduleInfo>, GitError> {
    let mut out = Vec::new();
    for info in list_submodules(git, workdir, cancel)? {
        push_nested(git, workdir, info, cancel, &mut out)?;
    }
    Ok(out)
}

/// Pushes `info` and then, when it is set up and not too deep, its own submodules.
/// `top` is the top repository's root; `info.path` is relative to it.
fn push_nested(
    git: &GitCli,
    top: &Path,
    info: SubmoduleInfo,
    cancel: &CancelToken,
    out: &mut Vec<SubmoduleInfo>,
) -> Result<(), GitError> {
    let set_up = matches!(
        info.state,
        SubmoduleState::UpToDate | SubmoduleState::Changed
    ) && info.checked_out.is_some();
    let descend = set_up && info.depth < MAX_NESTING;
    let (parent, depth) = (info.path.clone(), info.depth + 1);
    out.push(info);
    if !descend {
        return Ok(());
    }
    let children = match list_submodules(git, &top.join(&parent), cancel) {
        Ok(children) => children,
        Err(err) if err.kind == GitErrorKind::Cancelled => return Err(err),
        Err(_) => return Ok(()),
    };
    for mut child in children {
        child.path = parent.join(&child.path);
        child.superproject = parent.clone();
        child.depth = depth;
        push_nested(git, top, child, cancel, out)?;
    }
    Ok(())
}

/// Checks out the recorded commits (`git submodule update --init [--recursive] --
/// <paths>`), cloning uninitialised ones. Empty `paths` = all. A `submodule.<name>.update`
/// setting (`rebase`, `merge`, `none`) is honoured, as in a terminal.
pub fn update_submodules(
    git: &GitCli,
    workdir: &Path,
    paths: &[PathBuf],
    recursive: bool,
    cancel: &CancelToken,
) -> Result<(), GitError> {
    update_submodules_with_events(git, workdir, paths, recursive, cancel, |_| {})
}

/// [`update_submodules`] with `--progress`, handing every stderr record to `on_event`:
/// clone meters and lines such as `Cloning into '…'...` or `Submodule path 'x': checked
/// out '…'`.
pub fn update_submodules_with_events(
    git: &GitCli,
    workdir: &Path,
    paths: &[PathBuf],
    recursive: bool,
    cancel: &CancelToken,
    on_event: impl FnMut(StderrEvent),
) -> Result<(), GitError> {
    submodule_command(
        git,
        workdir,
        &["update", "--init", "--progress"],
        paths,
        recursive,
        cancel,
        on_event,
    )
}

/// Copies the URLs from `.gitmodules` into the config (`git submodule sync [--recursive]
/// -- <paths>`): `submodule.<name>.url` of initialised submodules, and the
/// `remote.origin.url` of checked-out ones. Empty `paths` = all.
pub fn sync_submodules(
    git: &GitCli,
    workdir: &Path,
    paths: &[PathBuf],
    recursive: bool,
    cancel: &CancelToken,
) -> Result<(), GitError> {
    submodule_command(git, workdir, &["sync"], paths, recursive, cancel, |_| {})
}

/// Adds a submodule (`git submodule add --progress [-b <branch>] -- <url> <path>`): clones
/// `url` into `path` (relative to the superproject root; it must not exist, or be an empty
/// folder) and stages the gitlink and `.gitmodules` for the user to commit. A relative `url`
/// (`../lib.git`) is resolved against the superproject's remote, as git does. `branch` is
/// checked out instead of the remote's HEAD and recorded as `submodule.<name>.branch`.
/// Clone progress arrives as events (git copies a plain local path without a meter).
///
/// `git submodule add` refuses `--end-of-options` (the `git submodule` script prints its
/// usage), so `--` is what keeps a URL starting with `-` from being read as an option.
/// Cloning from a local path or `file://` URL needs `protocol.file.allow=always` in the
/// user's config: git's default (`user`) doesn't cover submodule clones. When
/// `.git/modules/<name>` is left over from a removed submodule of the same name, git
/// refuses: `fatal: A git directory for '<name>' is found locally …`.
pub fn add_submodule(
    git: &GitCli,
    workdir: &Path,
    url: &str,
    path: &Path,
    branch: Option<&str>,
    cancel: &CancelToken,
    on_event: impl FnMut(StderrEvent),
) -> Result<(), GitError> {
    let mut command = git
        .command(workdir)
        .args(["submodule", "add", "--progress"])
        .env("GIT_LITERAL_PATHSPECS", "1");
    if let Some(branch) = branch {
        command = command.args(["-b", branch]);
    }
    command
        .arg("--")
        .arg(url)
        .arg(path.as_os_str())
        .run_with_events(cancel, on_event)?;
    Ok(())
}

/// Unregisters a submodule and empties its folder (`git submodule deinit [-f] -- <path>`):
/// its `submodule.<name>.*` settings leave `.git/config`, and [`list_submodules`] shows it
/// [`SubmoduleState::Uninitialized`]. `.gitmodules`, the gitlink and the submodule's
/// repository in `.git/modules/` stay, so [`update_submodules`] brings it back.
///
/// Without `force`, git refuses when the submodule's work tree has local changes, even
/// just an untracked file: [`GitErrorKind::SubmoduleModified`]. `force` discards them.
/// A submodule that isn't initialised is no error (git warns, exits 0).
pub fn deinit_submodule(
    git: &GitCli,
    workdir: &Path,
    path: &Path,
    force: bool,
    cancel: &CancelToken,
) -> Result<(), GitError> {
    let mut command = git
        .command(workdir)
        .args(["submodule", "deinit"])
        .env("GIT_LITERAL_PATHSPECS", "1");
    if force {
        command = command.arg("--force");
    }
    command.arg("--").arg(path.as_os_str()).run(cancel)?;
    Ok(())
}

/// Removes a submodule from the superproject: [`deinit_submodule`], then `git rm [-f] --
/// <path>`, which drops the gitlink, the folder and the `.gitmodules` section, staging the
/// removal for the user to commit. `git rm` alone would leave the `submodule.<name>.*`
/// settings in `.git/config`.
///
/// The submodule's repository stays in `.git/modules/<name>` (git never deletes it: it may
/// hold commits nobody pushed); adding a submodule of that name again then needs git's
/// `--force` or another `--name`. Local changes refuse the removal as in
/// [`deinit_submodule`], with nothing changed; `force` discards them.
pub fn remove_submodule(
    git: &GitCli,
    workdir: &Path,
    path: &Path,
    force: bool,
    cancel: &CancelToken,
) -> Result<(), GitError> {
    deinit_submodule(git, workdir, path, force, cancel)?;
    let mut command = git
        .command(workdir)
        .args(["rm", "-q"])
        .env("GIT_LITERAL_PATHSPECS", "1");
    if force {
        command = command.arg("-f");
    }
    command.arg("--").arg(path.as_os_str()).run(cancel)?;
    Ok(())
}

fn submodule_command(
    git: &GitCli,
    workdir: &Path,
    subcommand: &[&str],
    paths: &[PathBuf],
    recursive: bool,
    cancel: &CancelToken,
    on_event: impl FnMut(StderrEvent),
) -> Result<(), GitError> {
    let mut command = git
        .command(workdir)
        .arg("submodule")
        .args(subcommand)
        .env("GIT_LITERAL_PATHSPECS", "1");
    if recursive {
        command = command.arg("--recursive");
    }
    command
        .arg("--")
        .args(paths.iter().map(|p| p.as_os_str()))
        .run_with_events(cancel, on_event)?;
    Ok(())
}

/// What a submodule folder holds.
#[derive(Debug, PartialEq, Eq)]
enum Checkout {
    /// No repository (missing, empty, or a `.git` that doesn't lead to one).
    Missing,
    /// A repository without a commit.
    Unborn,
    At(ObjectId),
}

/// The HEAD commit of the repository in `folder`, if `folder` holds one.
fn head_of(git: &GitCli, folder: &Path, cancel: &CancelToken) -> Result<Checkout, GitError> {
    if std::fs::symlink_metadata(folder.join(".git")).is_err() {
        return Ok(Checkout::Missing);
    }
    let result = git
        .command(folder)
        .args(["rev-parse", "--verify", "--quiet", "HEAD^{commit}"])
        .read_only()
        // 1: an unborn HEAD.
        .allow_exit_codes(&[1])
        .run(cancel);
    match result {
        Ok(output) if output.status == 0 => {
            Ok(ObjectId::from_hex(output.stdout.trim_ascii())
                .map_or(Checkout::Missing, Checkout::At))
        }
        Ok(_) => Ok(Checkout::Unborn),
        Err(err) if err.kind == GitErrorKind::Cancelled => Err(err),
        // `fatal: not a git repository: …` and the like.
        Err(_) => Ok(Checkout::Missing),
    }
}

/// A gitlink in the index.
#[derive(Debug, PartialEq, Eq)]
struct Gitlink {
    path: PathBuf,
    /// The stage-0 id; `None` when conflicted.
    recorded: Option<ObjectId>,
    conflict: bool,
}

/// Parses `git ls-files --stage -z` (`<mode> <id> <stage>\t<path>\0`, sorted by path and
/// then stage), keeping gitlinks only, one per path.
fn parse_gitlinks(bytes: &[u8]) -> Vec<Gitlink> {
    let mut links: Vec<Gitlink> = Vec::new();
    for record in bytes.split(|&b| b == 0).filter(|r| !r.is_empty()) {
        let Some(tab) = record.iter().position(|&b| b == b'\t') else {
            continue;
        };
        let (meta, path) = (&record[..tab], &record[tab + 1..]);
        let mut fields = meta.split(|&b| b == b' ');
        let (Some(mode), Some(id), Some(stage)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if mode != b"160000" {
            continue;
        }
        let path = path_from_bytes(path).into_owned();
        let conflict = stage != b"0";
        match links.last_mut() {
            Some(last) if last.path == path => {
                last.conflict |= conflict;
                last.recorded = None;
            }
            _ => links.push(Gitlink {
                path,
                recorded: if conflict {
                    None
                } else {
                    ObjectId::from_hex(id).ok()
                },
                conflict,
            }),
        }
    }
    links
}

/// A `[submodule "<name>"]` section of `.gitmodules`.
#[derive(Debug, PartialEq, Eq)]
struct Module {
    name: String,
    path: Option<PathBuf>,
    url: Option<String>,
}

/// Parses `git config -z --get-regexp` output (`<key>\n<value>\0`) for
/// `submodule.<name>.(path|url)` keys, in file order. The name is everything between
/// `submodule.` and the last dot; a later value of the same key wins, like git does.
fn parse_gitmodules(bytes: &[u8]) -> Vec<Module> {
    let mut modules: Vec<Module> = Vec::new();
    let mut by_name: HashMap<Vec<u8>, usize> = HashMap::new();
    for record in bytes.split(|&b| b == 0).filter(|r| !r.is_empty()) {
        // A key without a value (`path` with no `=`) has no newline.
        let (key, value) = match record.iter().position(|&b| b == b'\n') {
            Some(newline) => (&record[..newline], Some(&record[newline + 1..])),
            None => (record, None),
        };
        let Some(rest) = key.strip_prefix(b"submodule.") else {
            continue;
        };
        let Some(dot) = rest.iter().rposition(|&b| b == b'.') else {
            continue;
        };
        let (name, variable) = (&rest[..dot], &rest[dot + 1..]);
        let index = *by_name.entry(name.to_vec()).or_insert_with(|| {
            modules.push(Module {
                name: String::from_utf8_lossy(name).into_owned(),
                path: None,
                url: None,
            });
            modules.len() - 1
        });
        let module = &mut modules[index];
        match variable {
            b"path" => module.path = value.map(|v| path_from_bytes(v).into_owned()),
            b"url" => module.url = value.map(|v| String::from_utf8_lossy(v).into_owned()),
            _ => {}
        }
    }
    modules
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "1c272101c5c26442cf2d04b6fbc29030cf072307";
    const B: &str = "8c99fa62183e38cce9ad624b0b37b7df617ee033";

    fn id(hex: &str) -> Option<ObjectId> {
        Some(ObjectId::from_hex(hex.as_bytes()).unwrap())
    }

    fn link(path: &[u8], recorded: Option<ObjectId>, conflict: bool) -> Gitlink {
        Gitlink {
            path: path_from_bytes(path).into_owned(),
            recorded,
            conflict,
        }
    }

    #[test]
    fn keeps_gitlinks_and_folds_conflict_stages() {
        let mut bytes = Vec::new();
        for (mode, hex, stage, path) in [
            ("100644", A, "0", &b".gitmodules"[..]),
            ("160000", A, "0", b"a b"),
            ("160000", B, "0", b"caf\xe9"),
            ("160000", A, "1", b"lib"),
            ("160000", A, "2", b"lib"),
            ("160000", B, "3", b"lib"),
            ("100644", B, "2", b"text"),
            ("100644", A, "3", b"text"),
            ("160000", B, "0", b"tab\there"),
        ] {
            bytes.extend_from_slice(format!("{mode} {hex} {stage}\t").as_bytes());
            bytes.extend_from_slice(path);
            bytes.push(0);
        }
        assert_eq!(
            parse_gitlinks(&bytes),
            [
                link(b"a b", id(A), false),
                link(b"caf\xe9", id(B), false),
                link(b"lib", None, true),
                link(b"tab\there", id(B), false),
            ]
        );
        assert!(parse_gitlinks(b"").is_empty());
    }

    #[test]
    fn a_conflict_with_only_one_side_is_still_a_conflict() {
        // Deleted on one side: stages 1 and 3 only.
        let bytes = format!("160000 {A} 1\tlib\x00160000 {B} 3\tlib\x00");
        assert_eq!(parse_gitlinks(bytes.as_bytes()), [link(b"lib", None, true)]);
    }

    #[test]
    fn splits_names_on_the_last_dot() {
        let bytes = b"submodule.a b.path\na b\0\
            submodule.a b.url\n../a.git\0\
            submodule.n.a.me.path\nvendor/caf\xe9\0\
            submodule.n.a.me.url\nfile:///x\0\
            submodule.no-url.path\nno-url\0\
            submodule.bare.path\0\
            submodule.dup.path\nold\0\
            submodule.dup.path\nnew\0";
        let module = |name: &str, path: Option<&[u8]>, url: Option<&str>| Module {
            name: name.into(),
            path: path.map(|p| path_from_bytes(p).into_owned()),
            url: url.map(Into::into),
        };
        assert_eq!(
            parse_gitmodules(bytes),
            [
                module("a b", Some(b"a b"), Some("../a.git")),
                module("n.a.me", Some(b"vendor/caf\xe9"), Some("file:///x")),
                module("no-url", Some(b"no-url"), None),
                module("bare", None, None),
                module("dup", Some(b"new"), None),
            ]
        );
        assert!(parse_gitmodules(b"").is_empty());
    }
}
