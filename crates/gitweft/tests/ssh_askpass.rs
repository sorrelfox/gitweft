//! End-to-end: a fetch over "ssh" whose key needs a passphrase. The ssh stand-in asks
//! through `$SSH_ASKPASS` exactly like OpenSSH does; the gitweft binary (helper mode)
//! relays the prompt to the in-process AskpassServer, and the answer unlocks the fetch.

use std::path::Path;
use std::thread;

use gitweft_git::askpass::{AskpassServer, PromptKind};
use gitweft_git::remote::{FetchTarget, fetch};
use gitweft_git::{CancelToken, GitErrorKind};
use gitweft_testkit::Sandbox;

const HELPER: &str = env!("CARGO_BIN_EXE_gitweft");

/// Asks for a passphrase via `$SSH_ASKPASS`; with the right one, runs the git command
/// that git asked ssh to run on the "server" (the last argument) locally.
const FAKE_SSH: &str = r#"#!/bin/sh
answer=$("$SSH_ASKPASS" "Enter passphrase for key '/home/test/.ssh/id_ed25519': ") || { echo "git@host: Permission denied (publickey)." >&2; exit 255; }
[ "$answer" = "open sesame" ] || { echo "git@host: Permission denied (publickey)." >&2; exit 255; }
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

/// A repository whose `origin` is a local bare repository reached through FAKE_SSH,
/// with git wired to an AskpassServer. Returns the commit the remote has.
fn setup(
    sb: &Sandbox,
    server: &AskpassServer,
) -> (std::path::PathBuf, gitweft_git::GitCli, String) {
    let seed = sb.init_repo("seed");
    let commit = sb.commit_file(&seed, "a.txt", "1\n", "first");
    let bare = sb.path().join("remote.git");
    sb.run(
        sb.path(),
        &[
            "clone",
            "-q",
            "--bare",
            seed.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    let repo = sb.init_repo("r");
    let url = format!("ssh://git.example.invalid{}", bare.display());
    sb.run(&repo, &["remote", "add", "origin", &url]);

    let fake_ssh = sb.path().join("fake-ssh");
    std::fs::write(&fake_ssh, FAKE_SSH).unwrap();
    gitweft_platform::fs::set_executable(&fake_ssh, true).unwrap();
    let mut git = sb.git().clone();
    for (key, value) in server.git_env(Path::new(HELPER)) {
        git.set_env(key, value);
    }
    git.set_env("GIT_SSH_COMMAND", fake_ssh);
    // Plain `ssh host command` invocations (no OpenSSH feature probing with -G).
    git.set_env("GIT_SSH_VARIANT", "simple");
    (repo, git, commit)
}

#[test]
fn ssh_key_passphrase_prompt_reaches_the_app_and_unlocks_the_fetch() {
    let sb = Sandbox::new();
    let dir = socket_dir();
    let (server, requests) = AskpassServer::start_in(dir.path()).unwrap();
    let (repo, git, commit) = setup(&sb, &server);

    let answerer = thread::spawn(move || {
        let request = requests.recv_blocking().unwrap();
        let seen = (request.kind(), request.prompt.clone());
        request.answer("open sesame".into());
        seen
    });
    fetch(
        &git,
        &repo,
        FetchTarget::Remote("origin"),
        false,
        &CancelToken::new(),
        |_| {},
    )
    .unwrap();

    let (kind, prompt) = answerer.join().unwrap();
    assert_eq!(kind, PromptKind::Secret);
    assert!(prompt.starts_with("Enter passphrase for key"), "{prompt}");
    assert_eq!(sb.run(&repo, &["rev-parse", "origin/main"]).trim(), commit);
}

#[test]
fn a_cancelled_passphrase_prompt_is_an_authentication_failure() {
    let sb = Sandbox::new();
    let dir = socket_dir();
    let (server, requests) = AskpassServer::start_in(dir.path()).unwrap();
    let (repo, git, _) = setup(&sb, &server);

    let answerer = thread::spawn(move || requests.recv_blocking().unwrap().cancel());
    let err = fetch(
        &git,
        &repo,
        FetchTarget::Remote("origin"),
        false,
        &CancelToken::new(),
        |_| {},
    )
    .unwrap_err();
    answerer.join().unwrap();
    assert_eq!(err.kind, GitErrorKind::AuthFailed, "{}", err.stderr);
}
