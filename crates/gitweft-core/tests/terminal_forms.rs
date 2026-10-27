//! Real terminals told to run a command: each is launched as `terminal_running` builds
//! it, with a command whose arguments hold a space and a quote, and must run it in the
//! folder given. Terminals open windows, so this is ignored in `cargo test`; run it where
//! they're installed and a display is at hand (a container that shares the desktop's
//! Wayland socket does):
//!
//!   GITWEFT_TERMINALS="kgx tilix foot" cargo test -p gitweft-core --test terminal_forms -- --ignored
//!
//! Each name in `GITWEFT_TERMINALS` (all known ones when unset) that isn't installed is
//! skipped with a note; a terminal command of one's own can be given as `name=command`.

use std::ffi::OsString;
use std::path::Path;
use std::time::{Duration, Instant};

use gitweft_core::external::{Environment, terminal_running};

const KNOWN: &[&str] = &[
    "konsole",
    "ptyxis",
    "kgx",
    "gnome-terminal",
    "xfce4-terminal",
    "mate-terminal",
    "tilix",
    "alacritty",
    "kitty",
    "foot",
    "wezterm",
    "xterm",
];

#[test]
#[ignore = "opens terminal windows: run with --ignored where they're installed"]
fn real_terminals_run_a_command_in_the_folder() {
    let env = Environment::current();
    let wanted = std::env::var("GITWEFT_TERMINALS").unwrap_or_else(|_| KNOWN.join(" "));
    let mut failures = Vec::new();
    let mut ran = 0;
    for entry in wanted.split_whitespace() {
        let (name, setting) = entry.split_once('=').unwrap_or((entry, entry));
        let program = setting.split_whitespace().next().unwrap_or(name);
        if !installed(program) {
            eprintln!("note: {name} isn't installed; skipped");
            continue;
        }
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("a folder");
        std::fs::create_dir(&dir).unwrap();
        let out = root.path().join("out.txt");
        // Where it ran, then its second argument as it arrived.
        let script = r#"pwd > "$1.part"; printf '%s' "$2" >> "$1.part"; mv "$1.part" "$1""#;
        let command: Vec<OsString> = ["sh", "-c", script, "sh"]
            .iter()
            .map(OsString::from)
            .chain([out.clone().into(), OsString::from("it's a b")])
            .collect();
        let launch = match terminal_running(setting, &dir, &command, &env) {
            Ok(launch) => launch,
            Err(err) => {
                failures.push(format!("{name}: {err:?}"));
                continue;
            }
        };
        eprintln!("{name}: {} {:?}", launch.program.display(), launch.args);
        #[allow(clippy::disallowed_methods)] // A terminal, not git.
        let mut child = std::process::Command::new(&launch.program)
            .args(&launch.args)
            .current_dir(&launch.cwd)
            .spawn()
            .unwrap();
        let got = wait_for(&out, Duration::from_secs(20));
        let _ = child.try_wait();
        ran += 1;
        let expected = format!("{}\nit's a b", dir.display());
        match got {
            Some(text) if text == expected => eprintln!("ok: {name}"),
            Some(text) => failures.push(format!("{name}: wrote {text:?}, not {expected:?}")),
            None => failures.push(format!("{name}: ran nothing within 20 s")),
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
    assert!(ran > 0, "no terminal of {wanted:?} is installed");
}

fn installed(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()))
}

fn wait_for(file: &Path, limit: Duration) -> Option<String> {
    let start = Instant::now();
    while start.elapsed() < limit {
        if let Ok(text) = std::fs::read_to_string(file) {
            return Some(text);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None
}
