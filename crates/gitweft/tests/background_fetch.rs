//! End-to-end: a background fetch never asks. The askpass wiring is the app's (the
//! gitweft binary in helper mode, an in-process AskpassServer), but the run is
//! non-interactive, so a prompt fails at once and nothing reaches the server.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use gitweft_git::askpass::AskpassServer;
use gitweft_git::remote::fetch_background;
use gitweft_git::{CancelToken, GitCli, GitErrorKind};
use gitweft_testkit::Sandbox;

const HELPER: &str = env!("CARGO_BIN_EXE_gitweft");

/// Asks for a passphrase via `$SSH_ASKPASS` like OpenSSH; refused, it fails like OpenSSH.
/// With an answer it would run the "server" side (the last argument) locally.
const FAKE_SSH: &str = r#"#!/bin/sh
answer=$("$SSH_ASKPASS" "Enter passphrase for key '/home/test/.ssh/id_ed25519': ") || { echo "git@host: Permission denied (publickey)." >&2; exit 255; }
for last; do :; done
exec sh -c "$last"
"#;

fn socket_dir() -> tempfile::TempDir {
    // Socket paths are limited to about 100 bytes; keep them short.
    let base = if cfg!(unix) {
        std::path::PathBuf::from("/tmp")
    } else {
        std::env::temp_dir()
    };
    tempfile::Builder::new()
        .prefix("gw")
        .tempdir_in(base)
        .unwrap()
}

fn executable(sb: &Sandbox, name: &str, script: &str) -> PathBuf {
    let path = sb.path().join(name);
    std::fs::write(&path, script).unwrap();
    gitweft_platform::fs::set_executable(&path, true).unwrap();
    path
}

/// A bare repository with one commit; returns its path.
fn bare_remote(sb: &Sandbox) -> PathBuf {
    let seed = sb.init_repo("seed");
    sb.commit_file(&seed, "a.txt", "1\n", "first");
    let bare = sb.path().join("remote.git");
    sb.run(
        sb.path(),
        &["clone", "-q", "--bare", "seed", bare.to_str().unwrap()],
    );
    bare
}

fn wired_git(sb: &Sandbox, server: &AskpassServer) -> GitCli {
    let mut git = sb.git().clone();
    for (key, value) in server.git_env(Path::new(HELPER)) {
        git.set_env(key, value);
    }
    git
}

#[test]
fn an_ssh_passphrase_prompt_fails_at_once_without_reaching_the_app() {
    let sb = Sandbox::new();
    let dir = socket_dir();
    let (server, requests) = AskpassServer::start_in(dir.path()).unwrap();
    let bare = bare_remote(&sb);
    let repo = sb.init_repo("r");
    let url = format!("ssh://git.example.invalid{}", bare.display());
    sb.run(&repo, &["remote", "add", "origin", &url]);

    let mut git = wired_git(&sb, &server);
    git.set_env("GIT_SSH_COMMAND", executable(&sb, "fake-ssh", FAKE_SSH));
    git.set_env("GIT_SSH_VARIANT", "simple");

    let started = Instant::now();
    let err = fetch_background(&git, &repo, &CancelToken::new()).unwrap_err();
    assert_eq!(err.kind, GitErrorKind::AuthFailed, "{}", err.stderr);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
    assert!(requests.try_recv().is_err(), "no prompt may reach the UI");
}

#[test]
fn a_credential_prompt_is_refused_but_a_helper_that_knows_the_password_answers() {
    let sb = Sandbox::new();
    let dir = socket_dir();
    let (server, requests) = AskpassServer::start_in(dir.path()).unwrap();
    let git = wired_git(&sb, &server);
    let repo = sb.init_repo("r");
    let fill = |git: &GitCli| {
        git.command(&repo)
            .args(["credential", "fill"])
            .stdin("protocol=https\nhost=git.example.invalid\n\n")
            .non_interactive()
            .run(&CancelToken::new())
    };

    let err = fill(&git).unwrap_err();
    assert_eq!(err.kind, GitErrorKind::AuthFailed, "{}", err.stderr);
    assert!(requests.try_recv().is_err(), "no prompt may reach the UI");

    sb.run(
        &repo,
        &[
            "config",
            "credential.helper",
            "!f() { echo username=me; echo password=stored; }; f",
        ],
    );
    let output = fill(&git).unwrap();
    assert!(output.stdout_str().contains("password=stored\n"));
    assert!(requests.try_recv().is_err());
}

#[test]
fn without_gitweft_wiring_the_users_own_askpass_is_not_run_either() {
    let sb = Sandbox::new();
    let log = sb.path().join("askpass.log");
    let logger = executable(
        &sb,
        "their-askpass",
        &format!(
            "#!/bin/sh\necho \"$1\" >> '{}'\necho secret\n",
            log.display()
        ),
    );
    let mut git = sb.git().clone();
    git.set_env("GIT_ASKPASS", &logger);
    git.set_env("SSH_ASKPASS", &logger);
    let repo = sb.init_repo("r");

    let err = git
        .command(&repo)
        .args(["credential", "fill"])
        .stdin("protocol=https\nhost=git.example.invalid\n\n")
        .non_interactive()
        .run(&CancelToken::new())
        .unwrap_err();
    assert_eq!(err.kind, GitErrorKind::AuthFailed, "{}", err.stderr);
    assert!(!log.exists(), "the user's askpass was run");
}

#[test]
fn a_background_fetch_from_a_local_remote_just_works() {
    let sb = Sandbox::new();
    let dir = socket_dir();
    let (server, requests) = AskpassServer::start_in(dir.path()).unwrap();
    let bare = bare_remote(&sb);
    let url = format!("file://{}", bare.display());
    sb.run(sb.path(), &["clone", "-q", &url, "work"]);
    let work = sb.path().join("work");
    let seed = sb.path().join("seed");
    let pushed = sb.commit_file(&seed, "b.txt", "2\n", "second");
    sb.run(&seed, &["push", "-q", bare.to_str().unwrap(), "main"]);
    // A branch deleted on the remote goes (`--prune`).
    sb.run(&work, &["update-ref", "refs/remotes/origin/stale", "HEAD"]);

    let output = fetch_background(&wired_git(&sb, &server), &work, &CancelToken::new()).unwrap();
    assert_eq!(output.stderr, "", "--quiet");
    assert_eq!(sb.run(&work, &["rev-parse", "origin/main"]).trim(), pushed);
    // Full names: `%(refname:short)` shortens `refs/remotes/origin/HEAD` to `origin`.
    assert_eq!(
        sb.run(
            &work,
            &["for-each-ref", "--format=%(refname)", "refs/remotes"]
        ),
        "refs/remotes/origin/HEAD\nrefs/remotes/origin/main\n"
    );
    assert!(requests.try_recv().is_err());
}
