//! Isolated git sandboxes: a temp dir with its own HOME and global config, fixed
//! identities and dates, and no access to the user's or system git configuration.

use std::path::{Path, PathBuf};

use gitweft_git::{CancelToken, GitCli, GitVersion};
use tempfile::TempDir;

pub const FIXED_DATE: &str = "2026-01-01T00:00:00Z";

/// `path` as a `file://` URL, the form git and git-lfs both take: `file:///tmp/r`, and on
/// Windows `file:///C:/Users/r`, with forward slashes.
pub fn file_url(path: &Path) -> String {
    let path = path.to_str().expect("a UTF-8 path").replace('\\', "/");
    if path.starts_with('/') {
        format!("file://{path}")
    } else {
        format!("file:///{path}")
    }
}

pub struct Sandbox {
    dir: TempDir,
    git: GitCli,
}

impl Default for Sandbox {
    fn default() -> Self {
        Self::new()
    }
}

impl Sandbox {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().expect("create temp dir");
        let home = dir.path();
        let mut git = GitCli::discover().expect("git must be installed to run tests");
        for (key, value) in [
            ("GIT_CONFIG_NOSYSTEM", "1"),
            ("GIT_AUTHOR_NAME", "Test Author"),
            ("GIT_AUTHOR_EMAIL", "author@example.invalid"),
            ("GIT_COMMITTER_NAME", "Test Author"),
            ("GIT_COMMITTER_EMAIL", "author@example.invalid"),
            ("GIT_AUTHOR_DATE", FIXED_DATE),
            ("GIT_COMMITTER_DATE", FIXED_DATE),
        ] {
            git.set_env(key, value);
        }
        git.set_env("HOME", home);
        git.set_env("XDG_CONFIG_HOME", home.join(".config"));
        git.set_env("GIT_CONFIG_GLOBAL", home.join(".gitconfig"));
        // Never discover a repository above the sandbox.
        git.set_env("GIT_CEILING_DIRECTORIES", home.parent().unwrap_or(home));
        Self { dir, git }
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    pub fn git(&self) -> &GitCli {
        &self.git
    }

    /// Runs git in `cwd` and returns stdout; panics with git's stderr on failure.
    pub fn run(&self, cwd: &Path, args: &[&str]) -> String {
        match self.git.command(cwd).args(args).run(&CancelToken::new()) {
            Ok(out) => out.stdout_str().into_owned(),
            Err(err) => panic!(
                "`{}` failed: {}\n{}",
                err.command,
                err.summary(),
                err.stderr
            ),
        }
    }

    /// Creates `name` as a new repository on branch `main`.
    pub fn init_repo(&self, name: &str) -> PathBuf {
        let repo = self.path().join(name);
        std::fs::create_dir_all(&repo).expect("create repo dir");
        self.run(&repo, &["init", "-q", "-b", "main"]);
        repo
    }

    /// Creates `name` as a new SHA-256 repository (`--object-format=sha256`) on branch
    /// `main`: its object ids are 64 hex digits and its empty tree differs from SHA-1's.
    pub fn init_sha256_repo(&self, name: &str) -> PathBuf {
        let repo = self.path().join(name);
        std::fs::create_dir_all(&repo).expect("create repo dir");
        self.run(
            &repo,
            &["init", "-q", "-b", "main", "--object-format=sha256"],
        );
        repo
    }

    /// Creates `name` as a new reftable repository (`--ref-format=reftable`) on branch
    /// `main`: its refs aren't files, and gix 0.87 can't read them. `None`, with a
    /// note, when git is older than 2.45, which has no reftable: skip the test.
    pub fn init_reftable_repo(&self, name: &str) -> Option<PathBuf> {
        if self.git.version() < GitVersion::new(2, 45, 0) {
            eprintln!(
                "note: git {} has no reftable; skipping this test",
                self.git.version()
            );
            return None;
        }
        let repo = self.path().join(name);
        std::fs::create_dir_all(&repo).expect("create repo dir");
        self.run(
            &repo,
            &["init", "-q", "-b", "main", "--ref-format=reftable"],
        );
        Some(repo)
    }

    /// Writes `file` and commits it; returns the new commit id.
    pub fn commit_file(&self, repo: &Path, file: &str, content: &str, message: &str) -> String {
        let path = repo.join(file);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent dirs");
        }
        std::fs::write(&path, content).expect("write file");
        self.run(repo, &["add", "--", file]);
        self.run(repo, &["commit", "-q", "-m", message]);
        self.run(repo, &["rev-parse", "HEAD"]).trim().to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_urls_have_an_empty_host_and_forward_slashes() {
        assert_eq!(file_url(Path::new("/tmp/r.git")), "file:///tmp/r.git");
        if cfg!(windows) {
            assert_eq!(
                file_url(Path::new(r"C:\Users\me\r.git")),
                "file:///C:/Users/me/r.git"
            );
        }
    }
}
