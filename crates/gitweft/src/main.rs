//! Entry point. The mode is decided from argv *before* any GUI initialisation, so
//! helper modes (and the headless smoke test) never touch the display or the GPU.

mod smoke;

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use gitweft_workspace::cli_text;

/// glibc gives threads that allocate at the same time arenas of their own, up to eight per
/// core, and freed memory stays in them. On the Linux kernel's history the job threads'
/// arenas kept about 25 MB resident after the load; two arenas keep it to a few, with no
/// measurable cost to load time (#64). Called first, before any thread starts. A limit the
/// user set (`MALLOC_ARENA_MAX`, or `arena_max` in `GLIBC_TUNABLES`) is left alone.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn limit_malloc_arenas() {
    let tunables = std::env::var_os("GLIBC_TUNABLES").unwrap_or_default();
    if std::env::var_os("MALLOC_ARENA_MAX").is_some()
        || tunables.to_string_lossy().contains("arena_max")
    {
        return;
    }
    // SAFETY: no other thread exists yet; mallopt only changes the allocator's settings.
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, 2);
    }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn limit_malloc_arenas() {}

fn main() -> ExitCode {
    let started = std::time::Instant::now();
    limit_malloc_arenas();
    // git runs us as GIT_SEQUENCE_EDITOR during an interactive rebase: `--sequence-editor
    // <plan> <todo>`. Checked first, because the askpass variable below is set in every
    // git child's environment, including this one.
    let mut argv = std::env::args_os().skip(1);
    if argv.next().as_deref() == Some(std::ffi::OsStr::new("--sequence-editor")) {
        let (Some(plan), Some(todo)) = (argv.next(), argv.next()) else {
            eprintln!("gitweft: --sequence-editor needs a plan and a todo file");
            return ExitCode::from(2);
        };
        return match gitweft_git::rebase::run_sequence_editor(
            std::path::Path::new(&plan),
            std::path::Path::new(&todo),
        ) {
            Ok(()) => ExitCode::SUCCESS,
            Err(message) => {
                eprintln!("gitweft: {message}");
                ExitCode::FAILURE
            }
        };
    }
    // git/ssh run us as GIT_ASKPASS/SSH_ASKPASS with the prompt as the only argument;
    // the socket variable is only ever set in the environment of git children we spawn.
    if std::env::var_os(gitweft_git::askpass::ENV_SOCKET).is_some() {
        // `args_os`: `args` panics on a prompt that isn't UTF-8 (a Latin-1 user name in
        // a URL, #18); the prompt is only shown, so lossy text is enough.
        let prompt = std::env::args_os()
            .nth(1)
            .map(|prompt| prompt.to_string_lossy().into_owned())
            .unwrap_or_default();
        return gitweft_git::askpass::run_helper(&prompt);
    }

    match parse(std::env::args_os().skip(1)) {
        Err(flag) => {
            let flag = flag.to_string_lossy();
            eprint!(
                "{}\n\n{}",
                cli_text::unknown_option(&flag),
                cli_text::usage()
            );
            ExitCode::from(2)
        }
        Ok(Invocation::Version) => {
            println!("gitweft {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Ok(Invocation::Help) => {
            print!("{}", cli_text::usage());
            ExitCode::SUCCESS
        }
        Ok(Invocation::Smoke(path)) => smoke::run(path),
        Ok(Invocation::Open {
            new_instance,
            paths,
        }) => {
            // Absolute, so the running Gitweft (in another working directory) and the
            // session file see the same path.
            let paths: Vec<PathBuf> = paths
                .into_iter()
                .map(|arg| std::path::absolute(&arg).unwrap_or_else(|_| arg.into()))
                .collect();
            let mut other_unanswered = false;
            if !new_instance {
                match gitweft_core::ipc::send(&gitweft_core::ipc::socket_path(), &paths) {
                    Ok(()) => return ExitCode::SUCCESS,
                    // A Gitweft is there but hung: two windows, so say why (#22).
                    Err(err) if gitweft_core::ipc::is_no_answer(&err) => {
                        eprintln!("{}", cli_text::no_answer(&err));
                        other_unanswered = true;
                    }
                    Err(_) => {}
                }
            }
            // With a live Gitweft holding the socket, this one can't listen on it.
            let single_instance = !new_instance && !other_unanswered;
            if gitweft_workspace::run(paths, single_instance, started, other_unanswered) {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
    }
}

/// What the command line asks for.
#[derive(Debug, PartialEq, Eq)]
enum Invocation {
    Version,
    Help,
    /// `--headless-smoke [PATH]`.
    Smoke(Option<PathBuf>),
    /// The window, with a tab per path (as given, not yet absolute).
    Open {
        new_instance: bool,
        paths: Vec<OsString>,
    },
}

/// The arguments after the program's name. `--version`, `--help` and `--headless-smoke`
/// count only as the first argument; otherwise options and paths mix, and every argument
/// after `--` is a path, even one that starts with `-` (#22). An unknown option is the
/// error.
fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Invocation, OsString> {
    let mut args = args.into_iter().peekable();
    match args.peek().and_then(|arg| arg.to_str()) {
        Some("--version") => return Ok(Invocation::Version),
        Some("-h" | "--help") => return Ok(Invocation::Help),
        Some("--headless-smoke") => {
            args.next();
            return Ok(Invocation::Smoke(args.next().map(PathBuf::from)));
        }
        _ => {}
    }
    let mut new_instance = false;
    let mut paths = Vec::new();
    let mut only_paths = false;
    for arg in args {
        match arg.to_str() {
            _ if only_paths => paths.push(arg),
            Some("--") => only_paths = true,
            Some("--new-instance") => new_instance = true,
            // A lone `-` isn't an option either (and not a useful path, but git's tools take it).
            Some(flag) if flag.starts_with('-') && flag != "-" => return Err(arg),
            _ => paths.push(arg),
        }
    }
    Ok(Invocation::Open {
        new_instance,
        paths,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    fn open(new_instance: bool, paths: &[&str]) -> Invocation {
        Invocation::Open {
            new_instance,
            paths: args(paths),
        }
    }

    #[test]
    fn modes_are_the_first_argument_only() {
        assert_eq!(parse(args(&["--version"])), Ok(Invocation::Version));
        assert_eq!(parse(args(&["-h"])), Ok(Invocation::Help));
        assert_eq!(parse(args(&["--help", "x"])), Ok(Invocation::Help));
        assert_eq!(
            parse(args(&["--headless-smoke"])),
            Ok(Invocation::Smoke(None))
        );
        assert_eq!(
            parse(args(&["--headless-smoke", "repo"])),
            Ok(Invocation::Smoke(Some(PathBuf::from("repo"))))
        );
        // Later, they're unknown options.
        assert_eq!(
            parse(args(&["repo", "--version"])),
            Err(OsString::from("--version"))
        );
    }

    #[test]
    fn paths_and_the_new_instance_flag_mix() {
        assert_eq!(parse(args(&[])), Ok(open(false, &[])));
        assert_eq!(
            parse(args(&["a", "--new-instance", "b"])),
            Ok(open(true, &["a", "b"]))
        );
        assert_eq!(parse(args(&["-x", "a"])), Err(OsString::from("-x")));
    }

    #[test]
    fn everything_after_the_double_dash_is_a_path() {
        assert_eq!(
            parse(args(&[
                "--new-instance",
                "--",
                "-repo",
                "--new-instance",
                "--"
            ])),
            Ok(open(true, &["-repo", "--new-instance", "--"]))
        );
        assert_eq!(
            parse(args(&["--", "--version"])),
            Ok(open(false, &["--version"]))
        );
        assert_eq!(parse(args(&["-"])), Ok(open(false, &["-"])));
    }

    #[test]
    fn a_path_that_isnt_utf8_is_kept() {
        use std::os::unix::ffi::OsStringExt;
        let latin1 = OsString::from_vec(b"caf\xe9".to_vec());
        let dashed = OsString::from_vec(b"-caf\xe9".to_vec());
        assert_eq!(
            parse([latin1.clone(), OsString::from("--"), dashed.clone()]),
            Ok(Invocation::Open {
                new_instance: false,
                paths: vec![latin1, dashed],
            })
        );
    }
}
