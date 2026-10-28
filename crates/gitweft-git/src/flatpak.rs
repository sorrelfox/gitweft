//! In the Flatpak, git runs inside the sandbox, but the user's credential helpers and hooks
//! are written for the host: `git-credential-libsecret` isn't in the sandbox, and a hook
//! calling npm or python needs the host's. [`host_bridge`] sends both there through
//! `flatpak-spawn --host`: a credential helper that asks the host's `git credential`, and,
//! when the repository has hooks, a folder of wrappers that run each hook on the host.
//! Both are git config entries in the environment (`GIT_CONFIG_COUNT`), so they apply to
//! Gitweft's own git commands and nothing else.

use std::ffi::{OsStr, OsString};
use std::path::Path;

use gitweft_platform::bytes::path_from_bytes;
use gitweft_platform::fs;

use crate::{CancelToken, GitCli};

/// Runs a command on the host from inside a Flatpak sandbox.
pub const SPAWN: &str = "flatpak-spawn";

/// Whether this process runs in a Flatpak sandbox (Flatpak puts this file at its root).
pub fn in_flatpak() -> bool {
    Path::new("/.flatpak-info").exists()
}

/// The environment that sends credential helpers to the host (see [`host_bridge`]), for git
/// outside any repository (clone, init), where there are no hooks. Its entries follow the
/// ones `git` passes already.
pub fn credential_env(
    git: &GitCli,
    state: &Path,
    spawn: &Path,
) -> Result<Vec<(OsString, OsString)>, String> {
    let entries = credential_entries(state, spawn)?;
    Ok(config_env(first_index(git), &entries))
}

/// The environment that sends the credential helpers and hooks of the repository at `cwd`
/// to the host. `state`: a folder of Gitweft's where the scripts are written (rewritten
/// only when they change). `spawn`: `flatpak-spawn` ([`SPAWN`]; tests give a stand-in).
///
/// The hooks are the executable files, `.sample`s aside, in the folder git runs them from
/// (`git rev-parse --git-path hooks`: `core.hooksPath` or the repository's); without any,
/// git keeps its own folder. A hook added later is seen at the next load.
pub fn host_bridge(
    git: &GitCli,
    cwd: &Path,
    state: &Path,
    spawn: &Path,
    cancel: &CancelToken,
) -> Result<Vec<(OsString, OsString)>, String> {
    let mut entries = credential_entries(state, spawn)?;
    let output = git
        .command(cwd)
        .args(["rev-parse", "--git-path", "hooks"])
        .read_only()
        .run(cancel)
        .map_err(|e| e.summary())?;
    let listed = output.stdout.strip_suffix(b"\n").unwrap_or(&output.stdout);
    let hooks = cwd.join(path_from_bytes(listed));
    let names = hook_names(&hooks);
    if !names.is_empty() {
        let id = fnv1a(hooks.as_os_str().as_encoded_bytes());
        let dir = state.join("host-hooks").join(format!("{id:016x}"));
        write_hooks(&dir, &hooks, &names, spawn)?;
        entries.push(("core.hooksPath", dir.into_os_string()));
    }
    Ok(config_env(first_index(git), &entries))
}

/// Git's helpers cleared (an empty value resets the list: the user's run on the host), then
/// the one asking the host.
fn credential_entries(state: &Path, spawn: &Path) -> Result<Vec<(&'static str, OsString)>, String> {
    let helper = state.join("host-credential");
    write_script(&helper, &credential_script(spawn)?).map_err(|e| e.to_string())?;
    let helper = quote(&helper).ok_or("the helper's path isn't UTF-8")?;
    Ok(vec![
        ("credential.helper", OsString::new()),
        // `!`: run by the shell as it is (a quoted path would be taken as a helper's name).
        ("credential.helper", format!("!{helper}").into()),
    ])
}

/// The first free `GIT_CONFIG_KEY_<n>`: after the ones `git` passes already (its own
/// environment, which counts this process's too, else this process's). An earlier version
/// counted only the process's, so entries added to a handle that had some overwrote them.
fn first_index(git: &GitCli) -> usize {
    git.env_var("GIT_CONFIG_COUNT")
        .map(OsStr::to_owned)
        .or_else(|| std::env::var_os("GIT_CONFIG_COUNT"))
        .and_then(|count| count.to_str()?.parse().ok())
        .unwrap_or(0)
}

/// `entries` as git's environment config: `GIT_CONFIG_KEY_<n>` and `GIT_CONFIG_VALUE_<n>`
/// from `first` on, and the count.
fn config_env(first: usize, entries: &[(&str, OsString)]) -> Vec<(OsString, OsString)> {
    let mut env = Vec::new();
    for (i, (key, value)) in entries.iter().enumerate() {
        env.push((
            format!("GIT_CONFIG_KEY_{}", first + i).into(),
            (*key).into(),
        ));
        env.push((
            format!("GIT_CONFIG_VALUE_{}", first + i).into(),
            value.clone(),
        ));
    }
    let count = (first + entries.len()).to_string();
    env.push(("GIT_CONFIG_COUNT".into(), count.into()));
    env
}

/// The hooks in `dir`: executable files other than git's `.sample`s.
fn hook_names(dir: &Path) -> Vec<OsString> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<OsString> = entries
        .flatten()
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.as_encoded_bytes();
            !name.starts_with(b".")
                && !name.ends_with(b".sample")
                && std::fs::metadata(entry.path())
                    .is_ok_and(|meta| meta.is_file() && fs::is_executable(&meta))
        })
        .map(|entry| entry.file_name())
        .collect();
    names.sort();
    names
}

/// A wrapper per hook in `dir`; anything else there (a hook since removed) goes.
fn write_hooks(dir: &Path, hooks: &Path, names: &[OsString], spawn: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    for entry in std::fs::read_dir(dir).map_err(|e| e.to_string())?.flatten() {
        if !names.contains(&entry.file_name()) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    for name in names {
        let script = hook_script(&hooks.join(name), spawn)?;
        write_script(&dir.join(name), &script).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// The credential helper: git's `get`, `store` and `erase` are the host's `git credential
/// fill`, `approve` and `reject`, without prompts there: when the host knows nothing, the
/// helper fails and git goes on to Gitweft's prompt (a failing helper is skipped).
fn credential_script(spawn: &Path) -> Result<String, String> {
    let spawn = quote(spawn).ok_or("flatpak-spawn's path isn't UTF-8")?;
    Ok(format!(
        "#!/bin/sh\n\
         # Written by Gitweft in its Flatpak: the credential helpers are the host's, so the\n\
         # host's git answers, without prompts of its own.\n\
         case \"$1\" in\n\
         \x20   get) action=fill ;;\n\
         \x20   store) action=approve ;;\n\
         \x20   erase) action=reject ;;\n\
         \x20   *) exit 0 ;;\n\
         esac\n\
         exec {spawn} --host --watch-bus --env=GIT_TERMINAL_PROMPT=0 --env=GIT_ASKPASS= \\\n\
         \x20   --env=SSH_ASKPASS= git -c core.askPass= credential \"$action\" 2> /dev/null\n"
    ))
}

/// A hook's wrapper: the hook runs on the host, in the same folder, with git's `GIT_*`
/// variables (not Gitweft's config entries, nor the askpass helper, which is the sandbox's).
fn hook_script(hook: &Path, spawn: &Path) -> Result<String, String> {
    let spawn = quote(spawn).ok_or("flatpak-spawn's path isn't UTF-8")?;
    let hook = quote(hook).ok_or("the hook's path isn't UTF-8")?;
    Ok(format!(
        "#!/bin/bash\n\
         # Written by Gitweft in its Flatpak: runs the repository's hook on the host, where\n\
         # the programs it calls are.\n\
         args=(--host --watch-bus \"--directory=$PWD\")\n\
         while IFS= read -r -d '' var; do\n\
         \x20   case \"$var\" in\n\
         \x20       GIT_CONFIG_COUNT=* | GIT_CONFIG_KEY_* | GIT_CONFIG_VALUE_* | GIT_ASKPASS=*) ;;\n\
         \x20       GIT_*) args+=(\"--env=$var\") ;;\n\
         \x20   esac\n\
         done < <(env -0)\n\
         exec {spawn} \"${{args[@]}}\" {hook} \"$@\"\n"
    ))
}

/// Writes `script` at `path`, executable, unless it's there already. A new file is renamed
/// into place, so a git running the old one is never in the way.
fn write_script(path: &Path, script: &str) -> std::io::Result<()> {
    if std::fs::read(path).is_ok_and(|old| old == script.as_bytes()) {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut name = OsString::from(".");
    name.push(path.file_name().unwrap_or_default());
    name.push(format!(".{}.tmp", std::process::id()));
    let tmp = path.with_file_name(name);
    std::fs::write(&tmp, script)?;
    fs::set_executable(&tmp, true)?;
    std::fs::rename(&tmp, path)
}

/// `path` in single quotes for a shell; `None` when it isn't UTF-8.
fn quote(path: &Path) -> Option<String> {
    Some(format!("'{}'", path.to_str()?.replace('\'', r"'\''")))
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_follow_the_ones_the_handle_passes_already() {
        let mut git = GitCli::discover().unwrap();
        let state = std::env::temp_dir().join(format!("gitweft-flatpak-{}", std::process::id()));
        // A handle with two entries already, as after `credential_env`.
        git.set_env("GIT_CONFIG_COUNT", "2");
        let env = credential_env(&git, &state, Path::new("/usr/bin/flatpak-spawn")).unwrap();
        let _ = std::fs::remove_dir_all(&state);
        let keys: Vec<String> = env
            .iter()
            .map(|(key, _)| key.to_string_lossy().into_owned())
            .collect();
        assert!(keys.contains(&"GIT_CONFIG_KEY_2".to_owned()), "{keys:?}");
        assert!(!keys.contains(&"GIT_CONFIG_KEY_0".to_owned()), "{keys:?}");
        let count = env.last().map(|(key, value)| (key.clone(), value.clone()));
        assert_eq!(count, Some(("GIT_CONFIG_COUNT".into(), "4".into())));
    }

    #[test]
    fn config_entries_follow_the_ones_there() {
        let env = config_env(2, &[("a.b", "1".into()), ("c.d", OsString::new())]);
        let text: Vec<(String, String)> = env
            .into_iter()
            .map(|(k, v)| (k.into_string().unwrap(), v.into_string().unwrap()))
            .collect();
        let pairs = [
            ("GIT_CONFIG_KEY_2", "a.b"),
            ("GIT_CONFIG_VALUE_2", "1"),
            ("GIT_CONFIG_KEY_3", "c.d"),
            ("GIT_CONFIG_VALUE_3", ""),
            ("GIT_CONFIG_COUNT", "4"),
        ];
        assert_eq!(
            text,
            pairs.map(|(k, v)| (k.to_owned(), v.to_owned())).to_vec()
        );
    }

    #[test]
    fn paths_are_quoted_for_the_shell() {
        assert_eq!(
            quote(Path::new("/a b/it's")).as_deref(),
            Some(r"'/a b/it'\''s'")
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_path_that_is_not_utf8_is_not_quoted() {
        use std::os::unix::ffi::OsStrExt;
        assert_eq!(
            quote(Path::new(std::ffi::OsStr::from_bytes(b"/caf\xe9"))),
            None
        );
    }
}
