//! What Windows offers when nothing is set: Windows Terminal (else a console), Explorer
//! and Notepad. Windows hands a program one line of text, not a list of words, and some
//! programs read it in their own way: [`quote_arg`] writes the words so that cmd and
//! Explorer read them as meant, as well as every program that splits its line the usual way.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use gitweft_platform::bytes::os_string_from_vec;

use super::{Environment, Launch, LaunchError, from_setting};

/// Windows Terminal, by its program's name.
pub(super) const TERMINAL: &str = "wt";

/// The shells that come with Windows and run in a console. Told to run a command, they
/// leave it to a console of its own.
const CONSOLE_SHELLS: &[&str] = &["cmd", "powershell", "pwsh"];

/// The extensions Windows finds a program by, in its own order, when it's named without
/// one (the ones in `PATHEXT` that can be started directly).
const EXTENSIONS: &[&str] = &[".com", ".exe", ".bat", ".cmd"];

/// The files that may be the program `path`: itself when it has an extension, else it
/// with each of [`EXTENSIONS`].
pub(super) fn candidates(path: PathBuf) -> Vec<PathBuf> {
    if path.extension().is_some() {
        return vec![path];
    }
    EXTENSIONS
        .iter()
        .map(|extension| {
            let mut name = path.clone().into_os_string();
            name.push(extension);
            PathBuf::from(name)
        })
        .collect()
}

/// Windows Terminal when it's installed (it comes with Windows 11), else cmd.
pub(super) fn default_terminal(env: &Environment) -> Option<String> {
    [TERMINAL, "cmd"]
        .into_iter()
        .find(|name| env.find(name).is_some())
        .map(str::to_owned)
}

/// `path` with every `/` made a `\`: a path joined from a repository's own (`src/a.rs`)
/// has both, and Explorer takes a `/` for the start of an option.
fn native(path: &Path) -> OsString {
    let bytes = path.as_os_str().as_encoded_bytes();
    os_string_from_vec(
        bytes
            .iter()
            .map(|&byte| if byte == b'/' { b'\\' } else { byte })
            .collect(),
    )
}

/// `arg` for Windows Terminal, which reads a `;` anywhere on its line as the start of
/// another command (a tab, a pane); `\;` is a semicolon.
fn terminal_arg(arg: &OsString) -> OsString {
    let mut bytes = Vec::new();
    for &byte in arg.as_encoded_bytes() {
        if byte == b';' {
            bytes.push(b'\\');
        }
        bytes.push(byte);
    }
    os_string_from_vec(bytes)
}

/// The arguments that open Windows Terminal in `dir`.
pub(super) fn terminal_args(dir: &Path) -> Vec<OsString> {
    vec!["-d".into(), terminal_arg(&native(dir))]
}

/// `launch` started through cmd's `start`, which gives a console program a console window
/// of its own, as the Start menu does. Started directly it would have ours: standard input
/// from the null device, where a shell reads its end at once, and output nobody sees. A
/// program with windows of its own is started by `start` as it is.
///
/// cmd reads the line first. Every word but plain ones is quoted ([`quote_arg`]), which
/// keeps `&`, `|`, `^` and brackets as text, but `%NAME%` in a word is still replaced by
/// the variable of that name, if there is one: no quoting prevents that. `/D` leaves out
/// the user's AutoRun commands, `/V:OFF` the `!NAME!` form of the same; `start`'s first
/// quoted word is the window's title, hence the empty one.
pub(super) fn in_console(launch: Launch, env: &Environment) -> Result<Launch, LaunchError> {
    let cmd = env
        .find("cmd")
        .ok_or_else(|| LaunchError::NotFound("cmd".into()))?;
    let mut args: Vec<OsString> = ["/D", "/V:OFF", "/C", "start", ""]
        .into_iter()
        .map(OsString::from)
        .collect();
    args.push(launch.program.into());
    args.extend(launch.args);
    Ok(Launch {
        program: cmd,
        args,
        cwd: launch.cwd,
    })
}

/// A terminal Windows has (`name`, set as `setting`) running `command` in `dir`: a tab of
/// Windows Terminal, or for a console's shell a console of the command's own, which closes
/// when it ends as a terminal's `-e` does. `None` for any other terminal.
pub(super) fn terminal_running(
    name: &str,
    setting: &str,
    dir: &Path,
    command: &[OsString],
    env: &Environment,
) -> Result<Option<Launch>, LaunchError> {
    if name == TERMINAL {
        // Its options end at the first word that isn't one; the rest is the command.
        let mut launch = from_setting(setting, dir, false, dir, env)?;
        launch.args = terminal_args(dir);
        launch.args.extend(command.iter().map(terminal_arg));
        return Ok(Some(launch));
    }
    if !CONSOLE_SHELLS.contains(&name) {
        return Ok(None);
    }
    let (program, args) = command
        .split_first()
        .ok_or_else(|| LaunchError::NoCommandForm(name.to_owned()))?;
    let launch = Launch {
        program: program.into(),
        args: args.to_vec(),
        cwd: dir.to_owned(),
    };
    in_console(launch, env).map(Some)
}

/// Explorer showing `path`: a folder opened, a file selected in its folder. Explorer splits
/// its line at commas, so `/select,` stands on its own and a path with a comma is quoted
/// ([`quote_arg`] does both).
pub(super) fn file_manager(
    path: &Path,
    is_dir: bool,
    folder: &Path,
    env: &Environment,
) -> Result<Launch, LaunchError> {
    let program = env
        .find("explorer")
        .ok_or_else(|| LaunchError::NotFound("explorer".into()))?;
    let mut args = Vec::new();
    if !is_dir {
        args.push("/select,".into());
    }
    args.push(native(path));
    Ok(Launch {
        program,
        args,
        cwd: folder.to_owned(),
    })
}

/// `file` in Notepad. The file's own application would run a script (`.bat`, `.js`, `.py`
/// where Python is installed) rather than open it; another editor is a setting away.
pub(super) fn editor(
    file: &Path,
    workdir: &Path,
    env: &Environment,
) -> Result<Launch, LaunchError> {
    let program = env
        .find("notepad")
        .ok_or_else(|| LaunchError::NotFound("notepad".into()))?;
    Ok(Launch {
        program,
        args: vec![native(file)],
        cwd: workdir.to_owned(),
    })
}

/// Whether `program`'s exit code says nothing about how it went: Explorer exits with 1
/// even when it showed what it was asked to.
pub(super) fn exit_code_means_nothing(program: &Path) -> bool {
    let text = program.as_os_str().to_string_lossy();
    let name = text.rsplit(['/', '\\']).next().unwrap_or(&text);
    name.eq_ignore_ascii_case("explorer.exe") || name.eq_ignore_ascii_case("explorer")
}

/// `arg` as it's written on a Windows command line, to be read back by the rules programs
/// split their line by (`CommandLineToArgvW`'s). Anything but letters, digits and `-_./\:`
/// is quoted, more than those rules need: inside quotes cmd takes `&`, `|`, `^` and
/// brackets as text, and Explorer doesn't split at a comma. Explorer's own options
/// (`/select,`) stay bare, as it wants them.
#[cfg(any(windows, test))]
pub(super) fn quote_arg(arg: &str) -> String {
    let plain = |c: char| c.is_ascii_alphanumeric() || "-_./\\:".contains(c);
    let option = arg.starts_with('/') && arg.chars().all(|c| plain(c) || c == ',');
    if !arg.is_empty() && (option || arg.chars().all(plain)) {
        return arg.to_owned();
    }
    // Backslashes are as they are, unless a quote follows: then each is doubled, and the
    // quote escaped with one more.
    let mut quoted = String::from('"');
    let mut backslashes = 0;
    for c in arg.chars() {
        if c == '\\' {
            backslashes += 1;
            continue;
        }
        let escapes = if c == '"' {
            2 * backslashes + 1
        } else {
            backslashes
        };
        quoted.extend(std::iter::repeat_n('\\', escapes));
        quoted.push(c);
        backslashes = 0;
    }
    quoted.extend(std::iter::repeat_n('\\', 2 * backslashes));
    quoted.push('"');
    quoted
}

/// Gives `command` the arguments of `launch`, each written by [`quote_arg`]. A batch file
/// is run by cmd, whose quoting std knows, and a word that isn't Unicode is left to std too.
#[cfg(windows)]
pub(super) fn add_args(command: &mut std::process::Command, launch: &Launch) {
    use std::os::windows::process::CommandExt;

    let batch = launch.program.extension().is_some_and(|extension| {
        extension.eq_ignore_ascii_case("bat") || extension.eq_ignore_ascii_case("cmd")
    });
    for arg in &launch.args {
        match arg.to_str() {
            Some(text) if !batch => command.raw_arg(quote_arg(text)),
            _ => command.arg(arg),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{args, name, programs};
    use super::super::{
        System, default_terminal, editor, file_manager, terminal, terminal_running,
    };
    use super::*;

    fn on_windows(names: &[&str]) -> (tempfile::TempDir, Environment) {
        let (dir, mut env) = programs(names);
        env.system = System::Windows;
        (dir, env)
    }

    /// The line Windows gets for `launch`'s arguments.
    fn line(launch: &Launch) -> String {
        args(launch)
            .iter()
            .map(|arg| quote_arg(arg))
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn words(words: &[&str]) -> Vec<OsString> {
        words.iter().map(OsString::from).collect()
    }

    #[test]
    fn names_are_read_as_windows_reads_them() {
        let read = |word| super::super::program_name(word, System::Windows);
        assert_eq!(read(r"C:\Tools\WT.EXE"), "wt");
        assert_eq!(read("C:/Tools/Pwsh"), "pwsh");
        assert_eq!(read("cmd"), "cmd");
    }

    #[test]
    fn programs_are_found_by_their_extension() {
        let (bin, env) = on_windows(&["wt.exe", "code.cmd", "notes"]);
        assert_eq!(env.find("wt"), Some(bin.path().join("wt.exe")));
        assert_eq!(env.find("code"), Some(bin.path().join("code.cmd")));
        assert_eq!(env.find("wt.exe"), Some(bin.path().join("wt.exe")));
        // A file without one isn't a program there.
        assert_eq!(env.find("notes"), None);
        let named = bin.path().join("wt").display().to_string();
        assert_eq!(env.find(&named), Some(bin.path().join("wt.exe")));
    }

    #[test]
    fn windows_terminal_opens_in_the_folder() {
        let (_bin, env) = on_windows(&["wt.exe", "cmd.exe"]);
        let dir = Path::new("C:/r/R&D;x");
        assert_eq!(default_terminal(&env).as_deref(), Some("wt"));
        for setting in ["", "wt", "wt.exe"] {
            let launch = terminal(setting, dir, &env).unwrap();
            assert_eq!(
                (name(&launch), args(&launch)),
                ("wt.exe".into(), vec!["-d".into(), r"C:\r\R&D\;x".into()])
            );
            assert_eq!(launch.cwd, dir);
            assert_eq!(line(&launch), r#"-d "C:\r\R&D\;x""#);
        }
    }

    #[test]
    fn without_windows_terminal_cmd_gets_a_console() {
        let (bin, env) = on_windows(&["cmd.exe", "pwsh.exe"]);
        let cmd = bin.path().join("cmd.exe").display().to_string();
        let dir = Path::new("C:/r");
        assert_eq!(default_terminal(&env).as_deref(), Some("cmd"));
        let launch = terminal("", dir, &env).unwrap();
        assert_eq!(name(&launch), "cmd.exe");
        assert_eq!(
            args(&launch),
            ["/D", "/V:OFF", "/C", "start", "", cmd.as_str()]
        );
        // It starts in the folder: the console's cmd starts where `start` was.
        assert_eq!(launch.cwd, dir);
        // A console program of one's own gets one too, `{path}` as it is.
        let pwsh = bin.path().join("pwsh.exe").display().to_string();
        let launch = terminal("pwsh -NoLogo -WorkingDirectory {path}", dir, &env).unwrap();
        assert_eq!(
            args(&launch),
            [
                "/D",
                "/V:OFF",
                "/C",
                "start",
                "",
                pwsh.as_str(),
                "-NoLogo",
                "-WorkingDirectory",
                "C:/r"
            ]
        );
        let (_bin, bare) = on_windows(&[]);
        assert_eq!(terminal("", dir, &bare), Err(LaunchError::NoTerminal));
    }

    #[test]
    fn a_command_runs_in_a_tab_or_a_console_of_its_own() {
        let command = words(&["git", "mergetool", "--", "a b&c;d.txt"]);
        let dir = Path::new("C:/r");
        let (_bin, env) = on_windows(&["wt.exe", "cmd.exe"]);
        let launch = terminal_running("", dir, &command, &env).unwrap();
        assert_eq!(name(&launch), "wt.exe");
        assert_eq!(
            args(&launch),
            ["-d", r"C:\r", "git", "mergetool", "--", r"a b&c\;d.txt"]
        );
        assert_eq!(line(&launch), r#"-d C:\r git mergetool -- "a b&c\;d.txt""#);
        // cmd: the command itself in a console, each word that isn't plain quoted.
        for setting in ["cmd", "powershell"] {
            let launch = terminal_running(setting, dir, &command, &env).unwrap();
            assert_eq!(name(&launch), "cmd.exe");
            assert_eq!(launch.cwd, dir);
            assert_eq!(
                line(&launch),
                r#"/D /V:OFF /C start "" git mergetool -- "a b&c;d.txt""#
            );
        }
        // A command of one's own with `{command}` gets a console too.
        let (_bin, env) = on_windows(&["cmd.exe", "nu.exe"]);
        let launch = terminal_running("nu -c {command}", dir, &command, &env).unwrap();
        assert_eq!(name(&launch), "cmd.exe");
        assert!(args(&launch)[5].ends_with("nu.exe"), "{:?}", args(&launch));
        assert_eq!(
            args(&launch)[6..],
            ["-c", "git", "mergetool", "--", "a b&c;d.txt"]
        );
        // Another program can't be told what to run.
        assert_eq!(
            terminal_running("nu", dir, &command, &env),
            Err(LaunchError::NoCommandForm("nu".into()))
        );
    }

    #[test]
    fn explorer_shows_folders_and_selects_files() {
        let (_bin, env) = on_windows(&["explorer.exe"]);
        let file = Path::new("C:/r/a b,c.txt");
        let launch = file_manager("", file, false, &env).unwrap();
        assert_eq!(name(&launch), "explorer.exe");
        assert_eq!(args(&launch), ["/select,", r"C:\r\a b,c.txt"]);
        assert_eq!(launch.cwd, Path::new("C:/r"));
        assert_eq!(line(&launch), r#"/select, "C:\r\a b,c.txt""#);
        // A comma alone is quoted too, or Explorer would split there.
        let launch = file_manager("", Path::new("C:/r/a,b.txt"), false, &env).unwrap();
        assert_eq!(line(&launch), r#"/select, "C:\r\a,b.txt""#);
        let launch = file_manager("", Path::new("C:/r/src"), true, &env).unwrap();
        assert_eq!(line(&launch), r"C:\r\src");
        assert!(exit_code_means_nothing(&launch.program));
        assert!(exit_code_means_nothing(Path::new(
            r"C:\Windows\EXPLORER.EXE"
        )));
        assert!(!exit_code_means_nothing(Path::new(
            r"C:\Windows\notepad.exe"
        )));
    }

    #[test]
    fn files_open_in_notepad() {
        let (_bin, env) = on_windows(&["notepad.exe"]);
        let (file, workdir) = (Path::new("C:/r/src/main.rs"), Path::new("C:/r"));
        let launch = editor("", file, workdir, &env).unwrap();
        assert_eq!(
            (name(&launch), args(&launch)),
            ("notepad.exe".into(), vec![r"C:\r\src\main.rs".into()])
        );
        assert_eq!(launch.cwd, workdir);
        let (_bin, bare) = on_windows(&[]);
        assert_eq!(
            editor("", file, workdir, &bare),
            Err(LaunchError::NotFound("notepad".into()))
        );
    }

    #[test]
    fn a_setting_names_a_windows_path_in_single_quotes() {
        let (bin, env) = on_windows(&[]);
        let editor_dir = bin.path().join("Notepad++");
        std::fs::create_dir(&editor_dir).unwrap();
        std::fs::write(editor_dir.join("notepad++.exe"), "").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let program = editor_dir.join("notepad++.exe");
            std::fs::set_permissions(program, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let setting = format!("'{}' -n1 {{path}}", editor_dir.join("notepad++").display());
        let launch = editor(&setting, Path::new("C:/r/a.txt"), Path::new("C:/r"), &env).unwrap();
        assert_eq!(launch.program, editor_dir.join("notepad++.exe"));
        // `{path}` is expanded as it is everywhere.
        assert_eq!(args(&launch), ["-n1", "C:/r/a.txt"]);
    }

    #[test]
    fn words_are_quoted_for_the_command_line() {
        assert_eq!(quote_arg(r"C:\r\main.rs"), r"C:\r\main.rs");
        assert_eq!(quote_arg("--tool=vimdiff"), r#""--tool=vimdiff""#);
        assert_eq!(quote_arg(""), r#""""#);
        assert_eq!(quote_arg(r"C:\a b"), r#""C:\a b""#);
        // Backslashes before the closing quote, or a quote of the word's own, are doubled.
        assert_eq!(quote_arg(r"C:\a b\"), r#""C:\a b\\""#);
        assert_eq!(quote_arg(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(quote_arg(r#"a\"b"#), r#""a\\\"b""#);
        assert_eq!(quote_arg("R&D"), r#""R&D""#);
        assert_eq!(quote_arg("it's"), r#""it's""#);
        assert_eq!(quote_arg("/select,"), "/select,");
        assert_eq!(quote_arg("/V:OFF"), "/V:OFF");
        assert_eq!(quote_arg("a,b"), r#""a,b""#);
    }
}
