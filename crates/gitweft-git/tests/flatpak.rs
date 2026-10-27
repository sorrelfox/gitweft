//! The Flatpak bridge (credential helpers and hooks run on the host), with a stand-in for
//! `flatpak-spawn` whose "host" is this machine with a home and config of the sandbox's.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use gitweft_git::flatpak::{credential_env, host_bridge};
use gitweft_git::{CancelToken, GitCli};
use gitweft_testkit::Sandbox;

/// One test at a time: a script just written can't be run while another test's thread is
/// forking with it still open ("Text file busy").
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

/// The stand-in: the command runs with only `PATH`, the "host" home and global config
/// (`<sandbox>/host`), `HOST_MARK=yes` and the `--env` variables.
fn spawner(sb: &Sandbox) -> PathBuf {
    let host = sb.path().join("host");
    std::fs::create_dir_all(&host).unwrap();
    let program = sb.path().join("spawn-stub");
    let script = format!(
        "#!/bin/sh\n\
         [ -n \"$STUB_CLEAN\" ] || exec env -i STUB_CLEAN=1 PATH=\"$PATH\" HOME='{host}' \
         GIT_CONFIG_GLOBAL='{host}/.gitconfig' GIT_CONFIG_NOSYSTEM=1 HOST_MARK=yes \"$0\" \"$@\"\n\
         unset STUB_CLEAN\n\
         [ \"$1\" = --host ] || exit 64\n\
         shift\n\
         while :; do case \"$1\" in\n\
           --watch-bus) shift ;;\n\
           --directory=*) cd \"${{1#--directory=}}\" || exit 65; shift ;;\n\
           --env=*) export \"${{1#--env=}}\"; shift ;;\n\
           *) break ;;\n\
         esac; done\n\
         exec \"$@\"\n",
        host = host.display()
    );
    std::fs::write(&program, script).unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    program
}

/// The sandbox's git with the bridge's environment.
fn bridged(sb: &Sandbox, env: Vec<(std::ffi::OsString, std::ffi::OsString)>) -> GitCli {
    let mut git = sb.git().clone();
    for (key, value) in env {
        git.set_env(key, value);
    }
    git
}

fn write_hook(repo: &Path, name: &str, script: &str) {
    let path = repo.join(".git/hooks").join(name);
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn commit(git: &GitCli, repo: &Path, file: &str) -> Result<(), String> {
    std::fs::write(repo.join(file), "text\n").unwrap();
    let cancel = CancelToken::new();
    git.command(repo)
        .args(["add", file])
        .run(&cancel)
        .map_err(|e| e.summary())?;
    git.command(repo)
        .args(["commit", "-qm", file])
        .run(&cancel)
        .map_err(|e| e.stderr)?;
    Ok(())
}

#[test]
fn hooks_run_on_the_host_with_gits_variables() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    let out = sb.path().join("hook.out");
    let script = format!(
        "#!/bin/sh\necho \"mark=$HOST_MARK index=${{GIT_INDEX_FILE:+set}} pwd=$(pwd)\" > '{}'\n",
        out.display()
    );
    write_hook(&repo, "pre-commit", &script);
    let spawn = spawner(&sb);
    let state = sb.path().join("state");
    let env = host_bridge(sb.git(), &repo, &state, &spawn, &CancelToken::new()).unwrap();
    let git = bridged(&sb, env);

    commit(&git, &repo, "a.txt").unwrap();
    let seen = std::fs::read_to_string(&out).expect("the hook ran");
    assert_eq!(seen, format!("mark=yes index=set pwd={}\n", repo.display()));
    // Only the hooks there are wrapped.
    let wrappers: Vec<_> = std::fs::read_dir(state.join("host-hooks"))
        .unwrap()
        .flatten()
        .flat_map(|dir| std::fs::read_dir(dir.path()).unwrap().flatten())
        .map(|entry| entry.file_name().into_string().unwrap())
        .collect();
    assert_eq!(wrappers, ["pre-commit"]);
}

#[test]
fn a_failing_hook_on_the_host_stops_the_commit() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    write_hook(
        &repo,
        "pre-commit",
        "#!/bin/sh\necho \"refused on $HOST_MARK\" >&2\nexit 1\n",
    );
    let spawn = spawner(&sb);
    let env = host_bridge(
        sb.git(),
        &repo,
        &sb.path().join("state"),
        &spawn,
        &CancelToken::new(),
    )
    .unwrap();
    let git = bridged(&sb, env);

    let err = commit(&git, &repo, "a.txt").unwrap_err();
    assert!(err.contains("refused on yes"), "{err}");
}

#[test]
fn without_hooks_git_keeps_its_folder() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let sb = Sandbox::new();
    let repo = sb.init_repo("r"); // only git's .sample hooks
    let spawn = spawner(&sb);
    let env = host_bridge(
        sb.git(),
        &repo,
        &sb.path().join("state"),
        &spawn,
        &CancelToken::new(),
    )
    .unwrap();
    let values: Vec<String> = env
        .iter()
        .map(|(_, value)| value.to_string_lossy().into_owned())
        .collect();
    assert!(!values.iter().any(|v| v == "core.hooksPath"), "{values:?}");
    assert!(
        values.iter().any(|v| v == "credential.helper"),
        "{values:?}"
    );
    let git = bridged(&sb, env);
    commit(&git, &repo, "a.txt").unwrap();
}

#[test]
fn credentials_come_from_the_hosts_helpers() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let sb = Sandbox::new();
    let spawn = spawner(&sb);
    let host = sb.path().join("host");
    let store = host.join("creds");
    std::fs::write(&store, "https://ada:s3cret@h.invalid\n").unwrap();
    std::fs::write(
        host.join(".gitconfig"),
        format!(
            "[credential]\n\thelper = store --file={}\n",
            store.display()
        ),
    )
    .unwrap();
    let env = credential_env(sb.git(), &sb.path().join("state"), &spawn).unwrap();
    let git = bridged(&sb, env);
    let cancel = CancelToken::new();
    let credential = |action: &str, input: &str| {
        git.command(sb.path())
            .args(["credential", action])
            .stdin(input)
            // Git's own prompt: the program named here, which echoes the question.
            .env("GIT_ASKPASS", "echo")
            .run(&cancel)
            .unwrap()
            .stdout_str()
            .into_owned()
    };

    let known = credential("fill", "protocol=https\nhost=h.invalid\n\n");
    assert!(known.contains("username=ada\npassword=s3cret\n"), "{known}");
    // Unknown to the host: git goes on to its prompt.
    let unknown = credential("fill", "protocol=https\nhost=other.invalid\n\n");
    assert!(
        unknown.contains("username=Username for 'https://other.invalid'"),
        "{unknown}"
    );
    // A credential that worked is stored by the host's helper.
    credential(
        "approve",
        "protocol=https\nhost=new.invalid\nusername=bo\npassword=pw\n\n",
    );
    let stored = std::fs::read_to_string(&store).unwrap();
    assert!(stored.contains("https://bo:pw@new.invalid"), "{stored}");
}
