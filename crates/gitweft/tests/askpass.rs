//! End-to-end: real `git` asks for credentials, the gitweft binary (helper mode)
//! relays the prompts to an in-process AskpassServer, and the answers reach git.
//! `git credential fill` exercises the exact askpass path of fetch/push without a network.

use std::path::Path;
use std::thread;

use gitweft_git::askpass::{AskpassServer, PromptKind};
use gitweft_git::{CancelToken, GitErrorKind};
use gitweft_testkit::Sandbox;

const HELPER: &str = env!("CARGO_BIN_EXE_gitweft");
const QUERY: &str = "protocol=https\nhost=git.example.invalid\n\n";

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

#[test]
fn credentials_flow_through_the_helper() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    let dir = socket_dir();
    let (server, requests) = AskpassServer::start_in(dir.path()).unwrap();
    let mut git = sb.git().clone();
    for (key, value) in server.git_env(Path::new(HELPER)) {
        git.set_env(key, value);
    }

    let answerer = thread::spawn(move || {
        let mut seen = Vec::new();
        for _ in 0..2 {
            let req = requests.recv_blocking().unwrap();
            let answer = match req.kind() {
                PromptKind::Secret => "s3cret",
                _ => "alice",
            };
            seen.push((req.kind(), req.prompt.clone()));
            req.answer(answer.into());
        }
        seen
    });

    let out = git
        .command(&repo)
        .args(["credential", "fill"])
        .stdin(QUERY)
        .run(&CancelToken::new())
        .unwrap();
    let text = out.stdout_str();
    assert!(text.contains("username=alice"), "{text}");
    assert!(text.contains("password=s3cret"), "{text}");

    let seen = answerer.join().unwrap();
    assert_eq!(seen[0].0, PromptKind::Text);
    assert!(
        seen[0]
            .1
            .starts_with("Username for 'https://git.example.invalid'"),
        "{seen:?}"
    );
    assert_eq!(seen[1].0, PromptKind::Secret);
    assert!(
        seen[1]
            .1
            .starts_with("Password for 'https://alice@git.example.invalid'"),
        "{seen:?}"
    );
}

#[cfg(unix)]
#[test]
fn a_prompt_that_isnt_utf8_reaches_the_app() {
    use std::os::unix::ffi::OsStrExt as _;
    let dir = socket_dir();
    let (server, requests) = AskpassServer::start_in(dir.path()).unwrap();
    let answerer = thread::spawn(move || {
        let req = requests.recv_blocking().unwrap();
        let prompt = req.prompt.clone();
        req.answer("s3cret".into());
        prompt
    });

    // ssh runs SSH_ASKPASS with its prompt as the argument, and a key's path in it needn't
    // be UTF-8. (git's own prompts don't show this: it percent-encodes a URL's bytes.)
    // An earlier helper read the argument with `std::env::args`, which panics on that.
    let prompt = std::ffi::OsStr::from_bytes(b"Enter passphrase for key '/home/caf\xe9/id': ");
    #[allow(clippy::disallowed_methods)] // The helper binary under test, not git.
    let out = std::process::Command::new(HELPER)
        .arg(prompt)
        .envs(server.git_env(Path::new(HELPER)))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "s3cret");
    let seen = answerer.join().unwrap();
    assert!(seen.contains("caf\u{fffd}/id"), "{seen:?}");
}

#[test]
fn cancelled_prompt_fails_as_auth_error() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    let dir = socket_dir();
    let (server, requests) = AskpassServer::start_in(dir.path()).unwrap();
    let mut git = sb.git().clone();
    for (key, value) in server.git_env(Path::new(HELPER)) {
        git.set_env(key, value);
    }
    let answerer = thread::spawn(move || requests.recv_blocking().unwrap().cancel());

    let err = git
        .command(&repo)
        .args(["credential", "fill"])
        .stdin(QUERY)
        .run(&CancelToken::new())
        .unwrap_err();
    // Without a prompt the answerer would wait for ever; closing the server ends its wait.
    drop(server);
    answerer.join().unwrap();
    assert_eq!(err.kind, GitErrorKind::AuthFailed, "{}", err.stderr);
}

/// The helper's failures, each without asking anyone: prompts turned off for the
/// operation, a socket but no token (not started by Gitweft), nothing listening.
#[test]
fn the_helper_fails_without_asking_when_it_cant_or_mustnt() {
    use gitweft_git::askpass::{ENV_REFUSE, ENV_SOCKET, ENV_TOKEN};
    let dir = socket_dir();
    let socket = dir.path().join("nobody-listens");
    let run = |envs: &[(&str, &str)]| {
        #[allow(clippy::disallowed_methods)] // The helper binary under test, not git.
        let mut command = std::process::Command::new(HELPER);
        command
            .arg("Password: ")
            .env_remove(ENV_TOKEN)
            .env_remove(ENV_REFUSE)
            .env(ENV_SOCKET, &socket)
            .envs(envs.iter().copied());
        let out = command.output().unwrap();
        assert!(!out.status.success());
        assert!(out.stdout.is_empty(), "no answer");
        String::from_utf8_lossy(&out.stderr).into_owned()
    };

    let refused = run(&[(ENV_TOKEN, "t"), (ENV_REFUSE, "1")]);
    assert!(refused.contains("prompts are disabled"), "{refused}");
    let stray = run(&[]);
    assert!(stray.contains("not started by Gitweft"), "{stray}");
    let unreachable = run(&[(ENV_TOKEN, "t")]);
    assert!(
        unreachable.contains("cannot reach Gitweft"),
        "{unreachable}"
    );
}
