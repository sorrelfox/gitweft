//! Repository maintenance operations (local writes through the git CLI).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use gix::ObjectId;
use gix::bstr::BString;

use crate::cancel::CancelToken;
use crate::cli::{GitCli, GitError, GitErrorKind};
use crate::trash::{self, TrashAction, TrashFile, TrashFileKind, TrashRecord};

/// Writes the commit-graph file (with changed-path Bloom filters) that makes history
/// walks and path-limited log much faster. A purely local write inside `.git`.
pub fn write_commit_graph(
    git: &GitCli,
    git_dir: &Path,
    cancel: &CancelToken,
) -> Result<(), GitError> {
    let git_dir = std::path::absolute(git_dir)
        .map_err(|e| GitError::new(GitErrorKind::Other, "git commit-graph", e.to_string()))?;
    git.command(&git_dir)
        .env("GIT_DIR", &git_dir)
        .args(["commit-graph", "write", "--reachable", "--changed-paths"])
        .run(cancel)?;
    Ok(())
}

/// NUL-separated pathspec list for `--pathspec-from-file=- --pathspec-file-nul`:
/// no argv length limits, and (with `GIT_LITERAL_PATHSPECS`) no glob surprises for
/// files named like `*.txt`.
fn pathspec_stdin(paths: &[BString]) -> Vec<u8> {
    let mut stdin = Vec::new();
    for path in paths {
        stdin.extend_from_slice(path);
        stdin.push(0);
    }
    stdin
}

fn run_with_paths(
    git: &GitCli,
    workdir: &Path,
    args: &[&str],
    paths: &[BString],
    cancel: &CancelToken,
) -> Result<(), GitError> {
    if paths.is_empty() {
        return Ok(());
    }
    git.command(workdir)
        .args(args)
        .args(["--pathspec-from-file=-", "--pathspec-file-nul"])
        .env("GIT_LITERAL_PATHSPECS", "1")
        .stdin(pathspec_stdin(paths))
        .run(cancel)?;
    Ok(())
}

/// Stages the given paths completely: modifications, deletions and new files.
pub fn stage_paths(
    git: &GitCli,
    workdir: &Path,
    paths: &[BString],
    cancel: &CancelToken,
) -> Result<(), GitError> {
    run_with_paths(git, workdir, &["add", "--all"], paths, cancel)
}

/// Removes the given paths from the index (back to HEAD), keeping the working tree.
/// For a staged rename pass both the old and the new path.
pub fn unstage_paths(
    git: &GitCli,
    workdir: &Path,
    paths: &[BString],
    unborn: bool,
    cancel: &CancelToken,
) -> Result<(), GitError> {
    if unborn {
        // No HEAD to restore from: everything staged is new, so drop it from the index.
        run_with_paths(
            git,
            workdir,
            &["rm", "--cached", "-r", "-q", "--ignore-unmatch"],
            paths,
            cancel,
        )
    } else {
        run_with_paths(git, workdir, &["restore", "--staged"], paths, cancel)
    }
}

/// What [`discard_paths`] saved before throwing changes away.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiscardBackup {
    /// Tracked files: their discarded content, written to the object database.
    pub blobs: Vec<(BString, ObjectId)>,
    /// The trash entry that records this discard ([`crate::trash`]); the untracked files
    /// were moved to its `files/` folder. `None` when there was nothing to save.
    pub trash_dir: Option<PathBuf>,
}

/// Discards unstaged changes. Tracked files are restored from the index after their
/// current content is saved as blobs (`git hash-object -w`); untracked files are moved to
/// a new trash entry under `trash_root`, whose record ([`crate::trash::TrashRecord`]) also
/// lists the blobs. Nothing is lost for good.
pub fn discard_paths(
    git: &GitCli,
    workdir: &Path,
    tracked: &[BString],
    untracked: &[BString],
    trash_root: &Path,
    cancel: &CancelToken,
) -> Result<DiscardBackup, GitError> {
    discard_into_trash(
        git,
        workdir,
        tracked,
        untracked,
        trash_root,
        TrashAction::Discard,
        cancel,
    )
}

/// Cleans untracked or ignored files and folders (`git clean -n` candidates, folders
/// with their trailing `/`): moves them to a new trash entry recorded as a clean.
pub fn clean_paths(
    git: &GitCli,
    workdir: &Path,
    paths: &[BString],
    trash_root: &Path,
    cancel: &CancelToken,
) -> Result<DiscardBackup, GitError> {
    discard_into_trash(
        git,
        workdir,
        &[],
        paths,
        trash_root,
        TrashAction::Clean,
        cancel,
    )
}

fn discard_into_trash(
    git: &GitCli,
    workdir: &Path,
    tracked: &[BString],
    untracked: &[BString],
    trash_root: &Path,
    action: TrashAction,
    cancel: &CancelToken,
) -> Result<DiscardBackup, GitError> {
    let mut backup = DiscardBackup::default();

    // Back up tracked content that still exists (deleted files have nothing to save): a
    // file's bytes, or a symlink's target (a dangling one too).
    let existing: Vec<(&BString, WorktreeEntry)> = tracked
        .iter()
        .filter_map(|path| Some((path, worktree_entry(&workdir.join(path_of(path)))?)))
        .collect();
    let paths: Vec<&BString> = existing.iter().map(|(path, _)| *path).collect();
    let entries: Vec<WorktreeEntry> = existing.iter().map(|(_, entry)| *entry).collect();
    let ids = hash_paths(git, workdir, &paths, true, cancel)?;
    backup.blobs = paths.into_iter().cloned().zip(ids).collect();
    let restored = run_with_paths(git, workdir, &["restore", "--worktree"], tracked, cancel);
    if backup.blobs.is_empty() && untracked.is_empty() {
        return restored.map(|()| backup);
    }

    let time = trash::now_ms();
    let dir = trash::create_entry_dir(trash_root, action, time).map_err(trash_error)?;
    let mut record = TrashRecord::new(workdir, action, time);
    // From here on the discard has happened (maybe half way): the record gets written
    // even if the job is cancelled now.
    let done = CancelToken::new();
    let left = match restored {
        Ok(()) => left_behind(git, workdir, &backup.blobs, &done),
        Err(_) => vec![None; backup.blobs.len()],
    };
    push_tracked(&mut record, &backup.blobs, &left, &entries);

    let mut result = restored;
    if result.is_ok() {
        let files = dir.join(trash::FILES_DIR);
        for path in untracked {
            let moved = match trash::relative_path(path) {
                Some(relative) => trash::move_path(&workdir.join(relative), &files.join(relative))
                    .map_err(trash_error),
                None => Err(GitError::new(
                    GitErrorKind::Other,
                    "trash",
                    format!("unexpected path {path:?}"),
                )),
            };
            if let Err(err) = moved {
                result = Err(err);
                break;
            }
            record.files.push(TrashFile {
                path: path.strip_suffix(b"/").unwrap_or(path).into(),
                kind: TrashFileKind::Untracked,
            });
        }
    }
    let written = record.write(&dir).map_err(trash_error);
    // The blobs stay safe from `git gc` while the entry exists (the Trash page gives an
    // entry its ref again if this fails).
    let ids: Vec<ObjectId> = backup.blobs.iter().map(|(_, id)| *id).collect();
    let _ = trash::protect(git, workdir, &dir, &ids, &done);
    backup.trash_dir = Some(dir);
    result.and(written).map(|()| backup)
}

/// Records the blobs a partial discard saved with [`backup_worktree_file`], once the
/// discard is applied, so the Trash page lists it like a whole-file discard. Returns the
/// new trash entry, or `None` when there are no blobs.
pub fn record_discarded_blobs(
    git: &GitCli,
    workdir: &Path,
    blobs: &[(BString, ObjectId)],
    trash_root: &Path,
) -> Result<Option<PathBuf>, GitError> {
    if blobs.is_empty() {
        return Ok(None);
    }
    let time = trash::now_ms();
    let dir =
        trash::create_entry_dir(trash_root, TrashAction::Discard, time).map_err(trash_error)?;
    let mut record = TrashRecord::new(workdir, TrashAction::Discard, time);
    let left = left_behind(git, workdir, blobs, &CancelToken::new());
    let entries: Vec<WorktreeEntry> = blobs
        .iter()
        .map(|(path, _)| worktree_entry(&workdir.join(path_of(path))).unwrap_or_default())
        .collect();
    push_tracked(&mut record, blobs, &left, &entries);
    record.write(&dir).map_err(trash_error)?;
    let ids: Vec<ObjectId> = blobs.iter().map(|(_, id)| *id).collect();
    let _ = trash::protect(git, workdir, &dir, &ids, &CancelToken::new());
    Ok(Some(dir))
}

fn push_tracked(
    record: &mut TrashRecord,
    blobs: &[(BString, ObjectId)],
    left: &[Option<ObjectId>],
    entries: &[WorktreeEntry],
) {
    for (((path, blob), left), entry) in blobs.iter().zip(left).zip(entries) {
        record.files.push(TrashFile {
            path: path.clone(),
            kind: TrashFileKind::Tracked {
                blob: *blob,
                left: *left,
                executable: entry.executable,
                symlink: entry.symlink,
            },
        });
    }
}

/// What the discard left at each blob's path (hashed like the backup, not written), so
/// a restore can tell whether the file changed since.
fn left_behind(
    git: &GitCli,
    workdir: &Path,
    blobs: &[(BString, ObjectId)],
    cancel: &CancelToken,
) -> Vec<Option<ObjectId>> {
    let present: Vec<&BString> = blobs
        .iter()
        .map(|(path, _)| path)
        .filter(|path| worktree_entry(&workdir.join(path_of(path))).is_some())
        .collect();
    let ids = hash_paths(git, workdir, &present, false, cancel).unwrap_or_default();
    let found: HashMap<&BString, ObjectId> = present.into_iter().zip(ids).collect();
    blobs
        .iter()
        .map(|(path, _)| found.get(path).copied())
        .collect()
}

/// The blob ids of `paths` (in order) as git would store them, with `--no-filters`: a
/// file's bytes unchanged, a symlink's target text (as git stores a link: `--stdin-paths`
/// would follow it, and fails on a dangling one). `write` stores them in the object
/// database.
pub fn hash_paths(
    git: &GitCli,
    workdir: &Path,
    paths: &[&BString],
    write: bool,
    cancel: &CancelToken,
) -> Result<Vec<ObjectId>, GitError> {
    let links: Vec<Option<Vec<u8>>> = paths
        .iter()
        .map(|path| link_text(&workdir.join(path_of(path))))
        .collect();
    let files: Vec<&BString> = paths
        .iter()
        .zip(&links)
        .filter(|(_, link)| link.is_none())
        .map(|(path, _)| *path)
        .collect();
    let mut file_ids = hash_files(git, workdir, &files, write, cancel)?.into_iter();
    let mut ids = Vec::with_capacity(paths.len());
    for link in links {
        ids.push(match link {
            Some(target) => hash_bytes(git, workdir, target, write, cancel)?,
            // `hash_files` checked it returned one id per file.
            None => file_ids.next().expect("an id per file"),
        });
    }
    Ok(ids)
}

/// `git hash-object --no-filters -- <paths>` for regular files.
fn hash_files(
    git: &GitCli,
    workdir: &Path,
    paths: &[&BString],
    write: bool,
    cancel: &CancelToken,
) -> Result<Vec<ObjectId>, GitError> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    // As arguments, not `--stdin-paths`: git C-unquotes a line that starts with `"` there,
    // and a line can't hold a newline. Chunked to stay well under the argument limit.
    const CHUNK_BYTES: usize = 64 * 1024;
    let bad = |what: String| GitError::new(GitErrorKind::Other, "git hash-object", what);
    let mut ids = Vec::with_capacity(paths.len());
    let mut rest = paths;
    while !rest.is_empty() {
        let mut bytes = 0;
        let count = rest
            .iter()
            .take_while(|path| {
                bytes += path.len() + 1;
                bytes <= CHUNK_BYTES
            })
            .count()
            .max(1);
        let (chunk, after) = rest.split_at(count);
        rest = after;
        let mut command = git.command(workdir).arg("hash-object");
        command = if write {
            command.arg("-w")
        } else {
            command.read_only()
        };
        let output = command
            .args(["--no-filters", "--"])
            .args(chunk.iter().map(|path| path_of(path)))
            .run(cancel)?;
        for line in output
            .stdout
            .split(|b| *b == b'\n')
            .filter(|l| !l.is_empty())
        {
            ids.push(ObjectId::from_hex(line).map_err(|e| bad(e.to_string()))?);
        }
    }
    if ids.len() != paths.len() {
        return Err(bad(format!("{} ids for {} paths", ids.len(), paths.len())));
    }
    Ok(ids)
}

/// `git hash-object --stdin`: the blob id of `bytes` (a symlink's target).
fn hash_bytes(
    git: &GitCli,
    workdir: &Path,
    bytes: Vec<u8>,
    write: bool,
    cancel: &CancelToken,
) -> Result<ObjectId, GitError> {
    let mut command = git.command(workdir).arg("hash-object");
    command = if write {
        command.arg("-w")
    } else {
        command.read_only()
    };
    let output = command.arg("--stdin").stdin(bytes).run(cancel)?;
    ObjectId::from_hex(output.stdout.trim_ascii())
        .map_err(|e| GitError::new(GitErrorKind::Other, "git hash-object", e.to_string()))
}

/// The target of the symlink at `path`, as bytes; `None` when it isn't one.
fn link_text(path: &Path) -> Option<Vec<u8>> {
    use std::os::unix::ffi::OsStringExt;
    std::fs::symlink_metadata(path)
        .ok()
        .filter(|meta| meta.is_symlink())?;
    std::fs::read_link(path)
        .ok()
        .map(|target| target.into_os_string().into_vec())
}

/// What a tracked path holds in the working tree.
#[derive(Debug, Clone, Copy, Default)]
struct WorktreeEntry {
    executable: bool,
    symlink: bool,
}

/// A file (and whether it's executable) or a symlink (a dangling one too) at `path`;
/// `None` for nothing there, or a folder.
fn worktree_entry(path: &Path) -> Option<WorktreeEntry> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::symlink_metadata(path).ok()?;
    if meta.is_symlink() {
        Some(WorktreeEntry {
            executable: false,
            symlink: true,
        })
    } else if meta.is_file() {
        Some(WorktreeEntry {
            executable: meta.permissions().mode() & 0o111 != 0,
            symlink: false,
        })
    } else {
        None
    }
}

fn trash_error(e: std::io::Error) -> GitError {
    GitError::new(GitErrorKind::Other, "trash", e.to_string())
}

/// Applies a patch from [`crate::patch::build_patch`]. `cached` targets the index (stage /
/// unstage), otherwise the working tree (discard). `git apply` is all-or-nothing: if the
/// file changed since the diff was taken, it fails and changes nothing.
pub fn apply_patch(
    git: &GitCli,
    workdir: &Path,
    patch: Vec<u8>,
    cached: bool,
    reverse: bool,
    cancel: &CancelToken,
) -> Result<(), GitError> {
    let mut args = vec!["apply", "--recount", "--whitespace=nowarn"];
    if cached {
        args.push("--cached");
    }
    if reverse {
        args.push("--reverse");
    }
    args.push("-");
    git.command(workdir).args(args).stdin(patch).run(cancel)?;
    Ok(())
}

/// Saves the working-tree content of `path` into the object database and returns its
/// blob id, so a partial discard can always be undone. `None` if the file doesn't exist.
pub fn backup_worktree_file(
    git: &GitCli,
    workdir: &Path,
    path: &BString,
    cancel: &CancelToken,
) -> Result<Option<ObjectId>, GitError> {
    // A symlink's content is its target, as git stores it.
    if worktree_entry(&workdir.join(path_of(path))).is_none() {
        return Ok(None);
    }
    Ok(hash_paths(git, workdir, &[path], true, cancel)?
        .into_iter()
        .next())
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CommitOptions {
    /// Replace HEAD instead of adding a commit on top of it.
    pub amend: bool,
    /// Add a `Signed-off-by:` trailer.
    pub signoff: bool,
    /// Skip the pre-commit and commit-msg hooks.
    pub no_verify: bool,
}

/// Commits the index with `message`. Hooks, signing (GPG/SSH) and message cleanup all
/// follow the user's git configuration. Returns git's output, hook messages included
/// (on failure they are in the error's `stderr`).
pub fn commit(
    git: &GitCli,
    workdir: &Path,
    message: &str,
    options: CommitOptions,
    cancel: &CancelToken,
) -> Result<String, GitError> {
    let mut args = vec!["commit", "--file=-"];
    if options.amend {
        args.push("--amend");
    }
    if options.signoff {
        args.push("--signoff");
    }
    if options.no_verify {
        args.push("--no-verify");
    }
    let output = git
        .command(workdir)
        .args(args)
        .stdin(message.as_bytes().to_vec())
        .run(cancel)?;
    let mut text = output.stdout_str().into_owned();
    text.push_str(&output.stderr);
    Ok(text)
}

/// The full message of HEAD (to prefill an amend).
pub fn head_message(
    git: &GitCli,
    workdir: &Path,
    cancel: &CancelToken,
) -> Result<String, GitError> {
    let output = git
        .command(workdir)
        .args(["log", "-1", "--format=%B"])
        .read_only()
        .run(cancel)?;
    Ok(output.stdout_str().trim_end().to_owned())
}

/// `user.name` and `user.email` as git would use them for the repository at `workdir`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Identity {
    pub name: Option<String>,
    pub email: Option<String>,
    /// A value is git's own guess (from the account's full name or the hostname), not
    /// something the user configured or set in the environment: worth confirming.
    pub guessed: bool,
    /// Where git's configuration sets the name and the email; `None` for a value
    /// that comes from the environment or git's guess.
    pub name_origin: Option<ConfigOrigin>,
    pub email_origin: Option<ConfigOrigin>,
}

/// Where git read a configuration value (`git config --show-scope --show-origin`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigOrigin {
    /// `system`, `global`, `local`, `worktree` or `command` (a `-c` option).
    pub scope: String,
    /// The file as git names it: `.git/config` (relative to the repository),
    /// `/home/me/.gitconfig`, an included file; empty for the command line.
    pub file: String,
}

/// Environment variables that set the identity on purpose (`EMAIL` only when git would
/// otherwise guess the email). Set on the [`GitCli`] or inherited from this process.
const IDENTITY_ENV: [&str; 5] = [
    "GIT_AUTHOR_NAME",
    "GIT_AUTHOR_EMAIL",
    "GIT_COMMITTER_NAME",
    "GIT_COMMITTER_EMAIL",
    "EMAIL",
];

/// The identity from the configuration. Only when that lacks the name or the email does
/// git fill them in (`git var GIT_AUTHOR_IDENT`, which resolves exactly as a commit would:
/// environment, then git's guess); when git can't either, they stay `None`.
pub fn identity(git: &GitCli, workdir: &Path, cancel: &CancelToken) -> Identity {
    let get = |key: &str| {
        git.command(workdir)
            .args([
                "config",
                "-z",
                "--show-scope",
                "--show-origin",
                "--get",
                key,
            ])
            .read_only()
            .run(cancel)
            .ok()
            .and_then(|out| parse_config_origin(&out.stdout_str()))
            .filter(|(value, _)| !value.is_empty())
    };
    let (name, name_origin) = get("user.name").unzip();
    let (email, email_origin) = get("user.email").unzip();
    let mut identity = Identity {
        name,
        email,
        guessed: false,
        name_origin,
        email_origin,
    };
    if identity.name.is_some() && identity.email.is_some() {
        return identity;
    }
    let resolved = git
        .command(workdir)
        .args(["var", "GIT_AUTHOR_IDENT"])
        .read_only()
        .run(cancel)
        .ok()
        .and_then(|out| parse_ident(&out.stdout_str()));
    let Some((name, email)) = resolved else {
        return identity;
    };
    let mut filled = false;
    for (slot, value) in [(&mut identity.name, name), (&mut identity.email, email)] {
        if slot.is_none() && !value.is_empty() {
            *slot = Some(value);
            filled = true;
        }
    }
    let from_env = IDENTITY_ENV.iter().any(|key| {
        git.env_var(key)
            .map(ToOwned::to_owned)
            .or_else(|| std::env::var_os(key))
            .is_some_and(|value| !value.is_empty())
    });
    identity.guessed = filled && !from_env;
    identity
}

/// `git config -z --show-scope --show-origin --get`'s `scope\0origin\0value\0` → the value
/// (trimmed) and where it's set.
fn parse_config_origin(output: &str) -> Option<(String, ConfigOrigin)> {
    let mut fields = output.split('\0');
    let (scope, origin, value) = (fields.next()?, fields.next()?, fields.next()?);
    let file = match origin.split_once(':') {
        Some(("file", path)) => path,
        Some(("command line", _)) => "",
        _ => origin,
    };
    let origin = ConfigOrigin {
        scope: scope.to_owned(),
        file: file.to_owned(),
    };
    Some((value.trim().to_owned(), origin))
}

/// `Name <email> 1234567890 +0000` → `(Name, email)`.
fn parse_ident(line: &str) -> Option<(String, String)> {
    let line = line.trim();
    let open = line.find('<')?;
    let close = open + line[open..].find('>')?;
    Some((
        line[..open].trim().to_owned(),
        line[open + 1..close].trim().to_owned(),
    ))
}

/// What the commit box needs to know about the repository, read together in the
/// background status job.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommitContext {
    /// HEAD's full message (to prefill an amend); `None` on an unborn branch.
    pub head_message: Option<String>,
    pub identity: Identity,
    /// The `commit.template` message without comment lines: `(summary, description)`.
    pub template: Option<(String, String)>,
}

pub fn commit_context(
    git: &GitCli,
    workdir: &Path,
    has_head: bool,
    cancel: &CancelToken,
) -> CommitContext {
    CommitContext {
        head_message: has_head
            .then(|| head_message(git, workdir, cancel).ok())
            .flatten(),
        identity: identity(git, workdir, cancel),
        template: commit_template(git, workdir, cancel),
    }
}

/// The `commit.template` file (relative paths are relative to the working tree, like for
/// `git commit` run there), split into summary and description, comment lines removed.
pub fn commit_template(
    git: &GitCli,
    workdir: &Path,
    cancel: &CancelToken,
) -> Option<(String, String)> {
    let config = |args: &[&str]| {
        git.command(workdir)
            .args(["config"])
            .args(args)
            .read_only()
            .run(cancel)
            .ok()
            .map(|out| out.stdout_str().trim().to_owned())
            .filter(|value| !value.is_empty())
    };
    let path = PathBuf::from(config(&["--path", "--get", "commit.template"])?);
    let text = std::fs::read_to_string(workdir.join(path)).ok()?;
    // `auto` picks a character not used in the message; for a template that is still '#'.
    let comment = config(&["--get", "core.commentChar"])
        .filter(|c| c.chars().count() == 1)
        .and_then(|c| c.chars().next())
        .unwrap_or('#');
    split_template(&text, comment)
}

/// Drops comment lines and splits what is left into summary (first line) and
/// description (the rest, without the separating blank lines). `None` if nothing is left.
pub fn split_template(text: &str, comment: char) -> Option<(String, String)> {
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| !line.starts_with(comment))
        .collect();
    let mut rest = lines
        .iter()
        .map(|l| l.trim_end())
        .skip_while(|l| l.is_empty());
    let summary = rest.next()?.to_owned();
    let description = rest.collect::<Vec<_>>().join("\n").trim().to_owned();
    Some((summary, description))
}

/// Appends a `Co-authored-by:` trailer for everyone not yet credited in `message`. Joins
/// a trailer block that ends the message, or starts one after a blank line.
pub fn with_co_authors(message: &str, co_authors: &[crate::read::Person]) -> String {
    let message = message.trim_end();
    let mut trailers: Vec<String> = Vec::new();
    for person in co_authors {
        let line = format!("Co-authored-by: {}", person.ident());
        if !trailers.contains(&line) && !message.lines().any(|l| l.trim() == line) {
            trailers.push(line);
        }
    }
    if trailers.is_empty() {
        return message.to_owned();
    }
    // The summary line alone is never a trailer block, even if it reads `feat: x`.
    let ends_with_trailers = message
        .rsplit_once("\n\n")
        .is_some_and(|(_, last)| !last.is_empty() && last.lines().all(is_trailer));
    let separator = if ends_with_trailers { "\n" } else { "\n\n" };
    format!("{message}{separator}{}", trailers.join("\n"))
}

/// `Token: value`, where the token is letters, digits and dashes (git's trailer syntax).
fn is_trailer(line: &str) -> bool {
    line.split_once(": ").is_some_and(|(token, _)| {
        !token.is_empty() && token.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    })
}

/// A repository-relative path as a filesystem path (git paths are bytes on Unix).
fn path_of(path: &BString) -> &Path {
    use std::os::unix::ffi::OsStrExt;
    Path::new(std::ffi::OsStr::from_bytes(path))
}

#[cfg(test)]
mod tests {
    use super::{ConfigOrigin, parse_config_origin, parse_ident, split_template, with_co_authors};
    use crate::read::Person;

    #[test]
    fn idents_split_into_name_and_email() {
        let pair = |name: &str, email: &str| Some((name.to_owned(), email.to_owned()));
        assert_eq!(
            parse_ident("Ada Lovelace <ada@example.invalid> 1767225600 +0000\n"),
            pair("Ada Lovelace", "ada@example.invalid")
        );
        assert_eq!(
            parse_ident("ada <ada@host.example> 1 -0130"),
            pair("ada", "ada@host.example")
        );
        assert_eq!(parse_ident(" <> 0 +0000"), pair("", ""));
        assert_eq!(parse_ident("no ident here"), None);
        assert_eq!(parse_ident("Unclosed <email 0 +0000"), None);
    }

    #[test]
    fn config_values_come_with_their_scope_and_file() {
        let origin = |scope: &str, file: &str| ConfigOrigin {
            scope: scope.to_owned(),
            file: file.to_owned(),
        };
        assert_eq!(
            parse_config_origin("global\0file:/home/me/.git config\0Ada\n\0"),
            Some(("Ada".to_owned(), origin("global", "/home/me/.git config")))
        );
        assert_eq!(
            parse_config_origin("command\0command line:\0a@b\0"),
            Some(("a@b".to_owned(), origin("command", "")))
        );
        assert_eq!(
            parse_config_origin("local\0blob:HEAD:cfg\0x\0"),
            Some(("x".to_owned(), origin("local", "blob:HEAD:cfg")))
        );
        assert_eq!(parse_config_origin(""), None);
    }

    #[test]
    fn co_author_trailers_join_or_start_a_trailer_block() {
        let person = |name: &str| Person {
            name: name.to_owned(),
            email: format!("{}@example.invalid", name.to_lowercase()),
        };
        let (ada, bob) = (person("Ada"), person("Bob"));
        assert_eq!(
            with_co_authors("Fix bug", std::slice::from_ref(&ada)),
            "Fix bug\n\nCo-authored-by: Ada <ada@example.invalid>"
        );
        assert_eq!(
            with_co_authors(
                "Fix\n\nBody text.\n\nRefs: #12\n",
                &[ada.clone(), bob.clone()]
            ),
            "Fix\n\nBody text.\n\nRefs: #12\n\
             Co-authored-by: Ada <ada@example.invalid>\n\
             Co-authored-by: Bob <bob@example.invalid>"
        );
        // Already credited, or nobody to add: unchanged.
        let credited = "Fix\n\nCo-authored-by: Ada <ada@example.invalid>";
        assert_eq!(with_co_authors(credited, &[ada.clone(), ada]), credited);
        assert_eq!(with_co_authors("Only a summary", &[]), "Only a summary");
        // A conventional-commit summary is not a trailer block.
        assert_eq!(
            with_co_authors("feat: add x", &[bob]),
            "feat: add x\n\nCo-authored-by: Bob <bob@example.invalid>"
        );
    }

    #[test]
    fn templates_lose_comments_and_split_into_summary_and_body() {
        let text = "# Summary: imperative, max 50 chars\n\
                    feat: \n\
                    \n\
                    # Why this change?\n\
                    Why:\n\
                    \n\
                    Refs: #\n";
        assert_eq!(
            split_template(text, '#'),
            Some(("feat:".to_owned(), "Why:\n\nRefs: #".to_owned()))
        );
        // Only guidance comments: nothing to prefill.
        assert_eq!(split_template("# explain why\n# and how\n", '#'), None);
        // A custom comment character leaves '#' lines alone.
        assert_eq!(
            split_template("; note\n#123 fix\n", ';'),
            Some(("#123 fix".to_owned(), String::new()))
        );
    }
}
