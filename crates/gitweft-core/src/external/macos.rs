//! What macOS offers when nothing is set: Terminal, the Finder and the default text editor,
//! each reached through `open`, which starts an application the way the Finder does.

use std::ffi::OsString;
use std::path::Path;

use super::{Environment, Launch, LaunchError, shell_line};

/// The terminal that comes with macOS, by its application's name.
pub(super) const TERMINAL: &str = "Terminal";

/// Whether a terminal setting names Terminal, which isn't on `PATH` but is `open`ed by name.
pub(super) fn is_terminal(name: &str) -> bool {
    name.eq_ignore_ascii_case(TERMINAL) || name.eq_ignore_ascii_case("Terminal.app")
}

/// `open` with `args`, started in `cwd`.
fn open(args: Vec<OsString>, cwd: &Path, env: &Environment) -> Result<Launch, LaunchError> {
    let program = env
        .find("open")
        .ok_or_else(|| LaunchError::NotFound("open".into()))?;
    Ok(Launch {
        program,
        args,
        cwd: cwd.to_owned(),
    })
}

/// A new Terminal window in `dir`, which is how Terminal opens a folder it's given.
pub(super) fn terminal(dir: &Path, env: &Environment) -> Result<Launch, LaunchError> {
    open(vec!["-a".into(), TERMINAL.into(), dir.into()], dir, env)
}

/// A new Terminal window running `command` in `dir`. Terminal is told what to run over
/// AppleScript, as a line it types into the user's shell: the line goes to the folder and
/// `exec`s the command, so the window's shell ends with it. Each word is quoted for the
/// shell ([`shell_line`]), then the line as an AppleScript string.
pub(super) fn terminal_running(
    dir: &Path,
    command: &[OsString],
    env: &Environment,
) -> Result<Launch, LaunchError> {
    let no_form = || LaunchError::NoCommandForm(TERMINAL.into());
    let folder = shell_line(&[dir.into()]).ok_or_else(no_form)?;
    let line = shell_line(command).ok_or_else(no_form)?;
    let line = format!("cd {folder} && exec {line}");
    let program = env
        .find("osascript")
        .ok_or_else(|| LaunchError::NotFound("osascript".into()))?;
    let script = [
        "tell application \"Terminal\"".to_owned(),
        format!("do script {}", applescript_string(&line)),
        "activate".to_owned(),
        "end tell".to_owned(),
    ];
    let args = script
        .into_iter()
        .flat_map(|line| [OsString::from("-e"), line.into()])
        .collect();
    Ok(Launch {
        program,
        args,
        cwd: dir.to_owned(),
    })
}

/// `text` as an AppleScript string literal.
fn applescript_string(text: &str) -> String {
    let mut literal = String::from('"');
    for c in text.chars() {
        match c {
            '\\' => literal.push_str(r"\\"),
            '"' => literal.push_str("\\\""),
            '\n' => literal.push_str(r"\n"),
            '\r' => literal.push_str(r"\r"),
            '\t' => literal.push_str(r"\t"),
            c => literal.push(c),
        }
    }
    literal.push('"');
    literal
}

/// The Finder showing `path`: a folder opened, a file selected in its folder (`open -R`).
pub(super) fn file_manager(
    path: &Path,
    is_dir: bool,
    folder: &Path,
    env: &Environment,
) -> Result<Launch, LaunchError> {
    let mut args = Vec::new();
    if !is_dir {
        args.push("-R".into());
    }
    args.push(path.into());
    open(args, folder, env)
}

/// `file` in the default text editor (`open -t`: TextEdit unless the user chose another).
/// The file's own application would run a script or an app rather than open it.
pub(super) fn editor(
    file: &Path,
    workdir: &Path,
    env: &Environment,
) -> Result<Launch, LaunchError> {
    open(vec!["-t".into(), file.into()], workdir, env)
}

#[cfg(test)]
mod tests {
    use super::super::tests::{args, name, programs};
    use super::super::{
        System, default_terminal, editor, file_manager, terminal, terminal_running,
    };
    use super::*;

    fn mac(names: &[&str]) -> (tempfile::TempDir, Environment) {
        let (dir, mut env) = programs(names);
        env.system = System::MacOs;
        (dir, env)
    }

    #[test]
    fn terminal_opens_a_window_in_the_folder() {
        let (_bin, env) = mac(&["open", "kitty"]);
        let dir = Path::new("/Users/me/a b");
        assert_eq!(default_terminal(&env).as_deref(), Some("Terminal"));
        for setting in ["", "Terminal", "terminal.app"] {
            let launch = terminal(setting, dir, &env).unwrap();
            assert_eq!(
                (name(&launch), args(&launch)),
                (
                    "open".into(),
                    vec!["-a".into(), "Terminal".into(), "/Users/me/a b".into()]
                )
            );
            assert_eq!(launch.cwd, dir);
        }
        // Another terminal on `PATH` is opened as it is on Linux.
        assert_eq!(
            args(&terminal("kitty", dir, &env).unwrap()),
            ["--directory", "/Users/me/a b"]
        );
        let (_bin, bare) = mac(&[]);
        assert_eq!(
            terminal("", dir, &bare),
            Err(LaunchError::NotFound("open".into()))
        );
    }

    #[test]
    fn terminal_runs_a_command_as_one_quoted_line() {
        let (_bin, env) = mac(&["osascript"]);
        let dir = Path::new(r#"/Users/me/it's "q" \x"#);
        let command: Vec<OsString> = ["git", "mergetool", "--", "a b.txt"]
            .iter()
            .map(OsString::from)
            .collect();
        let launch = terminal_running("", dir, &command, &env).unwrap();
        assert_eq!(name(&launch), "osascript");
        assert_eq!(launch.cwd, dir);
        // The shell line: `cd '/Users/me/it'\''s "q" \x' && exec 'git' …`, then each
        // backslash and double quote escaped for AppleScript.
        assert_eq!(
            args(&launch),
            [
                "-e",
                "tell application \"Terminal\"",
                "-e",
                r#"do script "cd '/Users/me/it'\\''s \"q\" \\x' && exec 'git' 'mergetool' '--' 'a b.txt'""#,
                "-e",
                "activate",
                "-e",
                "end tell",
            ]
        );
        assert_eq!(
            terminal_running("Terminal", dir, &command, &env).unwrap(),
            launch
        );
    }

    #[test]
    fn applescript_strings_escape_what_ends_them() {
        assert_eq!(applescript_string(r#"a "b" \c"#), r#""a \"b\" \\c""#);
        assert_eq!(applescript_string("1\n2\t3"), r#""1\n2\t3""#);
    }

    #[test]
    fn the_finder_shows_folders_and_reveals_files() {
        let (_bin, env) = mac(&["open"]);
        let file = Path::new("/Users/me/r/a b.txt");
        let launch = file_manager("", file, false, &env).unwrap();
        assert_eq!(
            (name(&launch), args(&launch)),
            (
                "open".into(),
                vec!["-R".into(), "/Users/me/r/a b.txt".into()]
            )
        );
        assert_eq!(launch.cwd, Path::new("/Users/me/r"));
        let launch = file_manager("", Path::new("/Users/me/r"), true, &env).unwrap();
        assert_eq!(args(&launch), ["/Users/me/r"]);
    }

    #[test]
    fn files_open_in_the_text_editor() {
        let (_bin, env) = mac(&["open", "code"]);
        let (file, workdir) = (Path::new("/Users/me/r/main.rs"), Path::new("/Users/me/r"));
        let launch = editor("", file, workdir, &env).unwrap();
        assert_eq!(
            (name(&launch), args(&launch)),
            (
                "open".into(),
                vec!["-t".into(), "/Users/me/r/main.rs".into()]
            )
        );
        assert_eq!(launch.cwd, workdir);
        // A setting is used as on Linux.
        assert_eq!(
            args(&editor("code -g {path}:1", file, workdir, &env).unwrap()),
            ["-g", "/Users/me/r/main.rs:1"]
        );
    }
}
