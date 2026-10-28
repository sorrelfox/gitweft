//! Getting past git's refusals: a folder git doesn't trust (another user owns it), a lock
//! file a crashed git left behind, and a commit identity git doesn't know.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use gitweft_platform::fs::canonicalize;
pub use gitweft_platform::process::LockHolder;
use gitweft_platform::process::lock_holders;

use crate::cancel::CancelToken;
use crate::cli::{GitCli, GitError};

/// The folder git named in a dubious-ownership refusal:
/// `fatal: detected dubious ownership in repository at '/path'` → `/path`, as git printed
/// it. A path with quotes in it runs up to the last `'` on that line.
pub fn untrusted_path(stderr: &str) -> Option<PathBuf> {
    const MARK: &str = "detected dubious ownership in repository at '";
    let line = stderr.lines().find(|line| line.contains(MARK))?;
    let rest = &line[line.find(MARK)? + MARK.len()..];
    let path = &rest[..rest.rfind('\'')?];
    (!path.is_empty()).then(|| PathBuf::from(path))
}

/// [`untrusted_path`], matched against the folder that was opened (`opened`, which may be
/// a subfolder): git prints the path as text, so a name that isn't UTF-8 reads back
/// changed, and the opened folder or the parent of it that prints the same is the real
/// one. Without a match, the path as printed.
pub fn untrusted_folder(stderr: &str, opened: Option<&Path>) -> Option<PathBuf> {
    let printed = untrusted_path(stderr)?;
    let printed_text = printed.to_string_lossy();
    let opened = opened.map(|path| canonicalize(path).unwrap_or_else(|_| path.to_owned()));
    let real = opened
        .iter()
        .flat_map(|path| path.ancestors())
        .find(|dir| dir.to_string_lossy() == printed_text)
        .map(Path::to_owned);
    Some(real.unwrap_or(printed))
}

/// Adds `path` to the global `safe.directory` list
/// (`git config --global --add safe.directory <path>`), unless it is listed already.
/// Runs outside any repository, since git refuses to work inside the untrusted one.
pub fn trust_directory(git: &GitCli, path: &Path, cancel: &CancelToken) -> Result<(), GitError> {
    let outside = Path::new("/");
    // Exit code 1: no entries at all.
    let listed = git
        .command(outside)
        .args(["config", "--global", "-z", "--get-all", "safe.directory"])
        .read_only()
        .allow_exit_codes(&[1])
        .run(cancel)?;
    let wanted = path.as_os_str().as_encoded_bytes();
    if listed
        .stdout
        .split(|b| *b == 0)
        .any(|entry| entry == wanted)
    {
        return Ok(());
    }
    git.command(outside)
        .args(["config", "--global", "--add", "safe.directory"])
        .arg(path)
        .run(cancel)?;
    Ok(())
}

/// What [`inspect_lock`] found out about a lock file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockInfo {
    pub path: PathBuf,
    /// The file is (still) there.
    pub exists: bool,
    /// How long ago it was last written, if that is known.
    pub age: Option<Duration>,
    /// Git processes that may be holding the lock now, by pid.
    pub holders: Vec<LockHolder>,
}

/// What we know about a lock file git reported: whether it (still) exists, how old it is
/// (from its mtime), and the git processes that may be holding it, as [`lock_holders`]
/// finds them: on Linux and macOS those of this user with their working directory inside
/// one of `dirs` (pass the working tree and the git dirs), on Windows those that have the
/// lock file open. Never fails: whatever can't be read is skipped.
pub fn inspect_lock(path: &Path, dirs: &[PathBuf]) -> LockInfo {
    let meta = std::fs::symlink_metadata(path).ok();
    let age = meta.as_ref().and_then(|m| m.modified().ok()).map(|mtime| {
        // A clock that went backwards (or a future mtime): just now.
        SystemTime::now()
            .duration_since(mtime)
            .unwrap_or(Duration::ZERO)
    });
    let dirs: Vec<PathBuf> = dirs.iter().filter_map(|d| canonicalize(d).ok()).collect();
    LockInfo {
        path: path.to_owned(),
        exists: meta.is_some(),
        age,
        holders: lock_holders(path, &dirs),
    }
}

/// Deletes a lock file a crashed git left behind. Refuses (`InvalidInput`) anything that
/// isn't a regular file named `*.lock` inside one of `git_dirs` (the git dir and the common
/// dir): the folders are compared canonicalized, and a symlink is never followed. A file
/// that is gone already is fine (someone else removed it).
pub fn remove_lock(path: &Path, git_dirs: &[PathBuf]) -> io::Result<()> {
    let refuse = |why: &str| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("won't remove {}: {why}", path.display()),
        )
    };
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(refuse("not a lock file"));
    };
    if !name.as_encoded_bytes().ends_with(b".lock") {
        return Err(refuse("not a lock file"));
    }
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    let parent = match canonicalize(parent) {
        Ok(parent) => parent,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    let inside = git_dirs
        .iter()
        .filter_map(|dir| canonicalize(dir).ok())
        .any(|dir| parent.starts_with(dir));
    if !inside {
        return Err(refuse("not inside the repository's git directory"));
    }
    let lock = parent.join(name);
    match std::fs::symlink_metadata(&lock) {
        Ok(meta) if meta.file_type().is_file() => {}
        Ok(_) => return Err(refuse("not a regular file")),
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    }
    match std::fs::remove_file(&lock) {
        Err(err) if err.kind() != io::ErrorKind::NotFound => Err(err),
        _ => Ok(()),
    }
}

/// Sets `user.name` and `user.email`: in the repository's own config, or with `global` in
/// the user's global config (replacing every value already there).
pub fn set_identity(
    git: &GitCli,
    workdir: &Path,
    name: &str,
    email: &str,
    global: bool,
    cancel: &CancelToken,
) -> Result<(), GitError> {
    // The global config needs no repository (and works while this one is refused).
    let (scope, cwd) = if global {
        ("--global", Path::new("/"))
    } else {
        ("--local", workdir)
    };
    for (key, value) in [("user.name", name), ("user.email", email)] {
        git.command(cwd)
            .args(["config", scope, "--replace-all", key, value])
            .run(cancel)?;
    }
    Ok(())
}

/// Sets an identity profile in the repository's own config: `user.name`,
/// `user.email`, and `user.signingKey` when the profile has one (a profile without one
/// leaves the repository's signing setup as it is).
pub fn apply_identity(
    git: &GitCli,
    workdir: &Path,
    name: &str,
    email: &str,
    signing_key: Option<&str>,
    cancel: &CancelToken,
) -> Result<(), GitError> {
    set_identity(git, workdir, name, email, false, cancel)?;
    if let Some(key) = signing_key {
        git.command(workdir)
            .args(["config", "--local", "--replace-all", "user.signingKey", key])
            .run(cancel)?;
    }
    Ok(())
}

/// The signing key git would use in the repository (`user.signingKey`, from any config),
/// for saving its identity as a profile.
pub fn signing_key(git: &GitCli, workdir: &Path, cancel: &CancelToken) -> Option<String> {
    git.command(workdir)
        .args(["config", "--get", "user.signingKey"])
        .read_only()
        .run(cancel)
        .ok()
        .map(|out| out.stdout_str().trim().to_owned())
        .filter(|key| !key.is_empty())
}

#[cfg(test)]
mod tests {
    use super::untrusted_path;
    use std::path::PathBuf;

    #[test]
    fn untrusted_path_is_the_quoted_folder() {
        // git 2.55's message.
        let stderr = "fatal: detected dubious ownership in repository at '/mnt/usb/my repo'\n\
                      To add an exception for this directory, call:\n\n\
                      \tgit config --global --add safe.directory '/mnt/usb/my repo'";
        assert_eq!(
            untrusted_path(stderr),
            Some(PathBuf::from("/mnt/usb/my repo"))
        );
        // Quotes inside the path: up to the last one on the line.
        assert_eq!(
            untrusted_path("fatal: detected dubious ownership in repository at '/a/it's/b'"),
            Some(PathBuf::from("/a/it's/b"))
        );
        assert_eq!(untrusted_path("fatal: not a git repository"), None);
        assert_eq!(
            untrusted_path("fatal: detected dubious ownership in repository at ''"),
            None
        );
    }
}
