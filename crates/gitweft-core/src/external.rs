//! The user's other programs: a terminal, the file manager and an editor, each started in
//! the background for a folder or file of a repository. Settings hold a command for each
//! (`{path}` stands for the folder or file); an empty one picks what the desktop offers.
//!
//! What it offers depends on the [`System`]: on Linux the desktop's own terminal and file
//! manager and `xdg-open`, on macOS Terminal, the Finder and the default text editor
//! (`macos`), on Windows Windows Terminal or a console, Explorer and Notepad (`windows`).
//!
//! A command setting is split into words the same way on every system ([`split_command`]):
//! a backslash escapes the next character there too, so a Windows path goes in single quotes
//! (`'C:\Program Files\Notepad++\notepad++.exe' {path}`) or is written with forward slashes.
//!
//! Git's own diff and merge tools run through git (`gitweft_git::tools`), not from here.
//!
//! Finding programs looks at `PATH` and a few files, so callers run it off the UI thread.
//!
//! In a Flatpak the user's programs are on the host, outside the sandbox: they're found and
//! started there through `flatpak-spawn --host` ([`Host`]).

use std::collections::HashMap;
use std::ffi::OsString;
use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use gitweft_platform::bytes::os_string_from_vec;

mod macos;
mod windows;

/// How long [`launch`] waits for a program that fails at once (a bad option, a missing file).
const QUICK_FAILURE: Duration = Duration::from_millis(1500);
/// Terminals tried in this order when nothing is set, after `$TERMINAL` and the desktop's own.
const TERMINALS: &[&str] = &[
    "konsole",
    "ptyxis",
    "kgx",
    "gnome-terminal",
    "xfce4-terminal",
    "mate-terminal",
    "tilix",
    "kitty",
    "alacritty",
    "foot",
    "wezterm",
    "xterm",
];

/// A program to start: what, with which arguments, in which folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchError {
    /// The command setting can't be read (an open quote) or is empty.
    BadCommand(String),
    /// The program isn't installed (or not on `PATH`).
    NotFound(String),
    /// No terminal set and none of the known ones installed.
    NoTerminal,
    /// It started but exited with an error at once (its last message).
    Failed { program: String, message: String },
    /// The terminal (named) can't be given a command to run: a command setting of the
    /// user's own, or a terminal whose option for that isn't known.
    NoCommandForm(String),
}

impl fmt::Display for LaunchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LaunchError::NoCommandForm(terminal) => {
                write!(f, "{terminal} can't be given a command to run")
            }
            LaunchError::BadCommand(command) => write!(f, "can't read the command `{command}`"),
            LaunchError::NotFound(program) => write!(f, "{program} was not found"),
            LaunchError::NoTerminal => write!(f, "no terminal was found"),
            LaunchError::Failed { program, message } => write!(f, "{program}: {message}"),
        }
    }
}

impl std::error::Error for LaunchError {}

/// The system whose programs are picked when nothing is set. Linux stands for the other
/// free desktops too (the BSDs), which offer the same.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum System {
    Linux,
    MacOs,
    Windows,
}

impl System {
    /// The system Gitweft runs on.
    pub const fn current() -> Self {
        if cfg!(target_os = "macos") {
            System::MacOs
        } else if cfg!(windows) {
            System::Windows
        } else {
            System::Linux
        }
    }
}

impl Default for System {
    fn default() -> Self {
        Self::current()
    }
}

/// What finding programs depends on, so tests can set it.
#[derive(Debug, Clone, Default)]
pub struct Environment {
    /// Which system's programs and ways are used.
    pub system: System,
    /// `PATH`.
    pub path: Option<OsString>,
    /// `$TERMINAL`, a common way to name one's terminal.
    pub terminal: Option<OsString>,
    /// `XDG_CURRENT_DESKTOP` (`KDE`, `GNOME`, `ubuntu:GNOME`…).
    pub desktop: Option<String>,
    /// `XDG_DATA_DIRS`, where D-Bus service files are.
    pub data_dirs: Option<OsString>,
    /// The desktop file of the default folder handler (`org.kde.dolphin.desktop`). `None`
    /// asks `xdg-mime` when it's needed; `Some(None)`: there is none.
    pub directory_handler: Option<Option<String>>,
    /// In a Flatpak: the programs are the host's, found and started through it (`path` and
    /// `data_dirs` are then the sandbox's and aren't used).
    pub host: Option<Host>,
}

/// The host, seen from a Flatpak sandbox.
#[derive(Debug, Clone)]
pub struct Host {
    /// Runs a command on the host: `flatpak-spawn` (tests give a stand-in).
    pub spawn: PathBuf,
    /// The host's programs found so far, by name.
    found: HashMap<String, PathBuf>,
    /// The names looked up so far, found or not.
    asked: Vec<String>,
}

/// The programs Gitweft may look for besides [`TERMINALS`]; the host is asked about them
/// all at once.
const HOST_PROGRAMS: &[&str] = &[
    "xdg-open",
    "xdg-mime",
    "dbus-send",
    "dolphin",
    "nautilus",
    "nemo",
];

impl Host {
    /// Looks up [`TERMINALS`], [`HOST_PROGRAMS`] and `more` on the host, in one call.
    pub fn new(spawn: PathBuf, more: &[String]) -> Self {
        let mut asked: Vec<String> = TERMINALS
            .iter()
            .chain(HOST_PROGRAMS)
            .map(|name| (*name).to_owned())
            .collect();
        asked.extend(more.iter().cloned());
        let found = host_lookup(&spawn, &asked);
        Self {
            spawn,
            found,
            asked,
        }
    }

    /// `name` on the host's `PATH` (a name with a slash is taken as it is: the sandbox
    /// can't check it).
    fn find(&self, name: &str) -> Option<PathBuf> {
        if name.contains('/') {
            return Some(PathBuf::from(name));
        }
        if let Some(path) = self.found.get(name) {
            return Some(path.clone());
        }
        if self.asked.iter().any(|asked| asked == name) {
            return None;
        }
        host_lookup(&self.spawn, &[name.to_owned()]).remove(name)
    }

    fn output(&self, args: &[&str]) -> Option<Vec<u8>> {
        run_on_host(&self.spawn, args)
    }
}

/// A command run on the host through `spawn`: its output when it succeeds.
fn run_on_host(spawn: &Path, args: &[&str]) -> Option<Vec<u8>> {
    #[allow(clippy::disallowed_methods)] // The host's programs, never git (see module docs).
    let output = Command::new(spawn)
        .arg("--host")
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    output.status.success().then_some(output.stdout)
}

/// `names` found on the host's `PATH`, with one `/bin/sh` there for all of them.
fn host_lookup(spawn: &Path, names: &[String]) -> HashMap<String, PathBuf> {
    const SCRIPT: &str = r#"for name do path=$(command -v -- "$name") && printf '%s\t%s\n' "$name" "$path"; done; exit 0"#;
    let mut args = vec!["/bin/sh", "-c", SCRIPT, "sh"];
    args.extend(names.iter().map(String::as_str));
    let Some(output) = run_on_host(spawn, &args) else {
        return HashMap::new();
    };
    String::from_utf8_lossy(&output)
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .filter(|(_, path)| path.starts_with('/'))
        .map(|(name, path)| (name.to_owned(), PathBuf::from(path)))
        .collect()
}

impl Environment {
    pub fn current() -> Self {
        let system = System::current();
        let terminal = std::env::var_os("TERMINAL").filter(|value| !value.is_empty());
        let in_flatpak = system == System::Linux && gitweft_git::flatpak::in_flatpak();
        let host = in_flatpak.then(|| {
            let named: Vec<String> = terminal
                .as_ref()
                .and_then(|value| split_command(&value.to_string_lossy()).ok())
                .map(|words| vec![words[0].clone()])
                .unwrap_or_default();
            Host::new("flatpak-spawn".into(), &named)
        });
        Self {
            system,
            path: std::env::var_os("PATH"),
            terminal,
            desktop: std::env::var("XDG_CURRENT_DESKTOP").ok(),
            data_dirs: std::env::var_os("XDG_DATA_DIRS"),
            directory_handler: None,
            host,
        }
    }

    /// The default folder handler's desktop file, from `xdg-mime` unless it's known.
    fn directory_handler(&self) -> Option<String> {
        if let Some(known) = &self.directory_handler {
            return known.clone();
        }
        let query = ["query", "default", "inode/directory"];
        let stdout = match &self.host {
            Some(host) => host.output(&[&["xdg-mime"][..], &query].concat())?,
            None => {
                let program = self.find("xdg-mime")?;
                #[allow(clippy::disallowed_methods)]
                // A desktop query, never git (see module docs).
                let output = Command::new(program)
                    .args(query)
                    .stdin(Stdio::null())
                    .stderr(Stdio::null())
                    .output()
                    .ok()?;
                output.status.success().then_some(output.stdout)?
            }
        };
        let id = String::from_utf8_lossy(&stdout).trim().to_owned();
        (!id.is_empty()).then_some(id)
    }

    /// `launch` as it's started from here: on the host, in its folder there, when in a
    /// Flatpak.
    fn on_host(&self, launch: Launch) -> Launch {
        let Some(host) = &self.host else {
            return launch;
        };
        let mut directory = OsString::from("--directory=");
        directory.push(&launch.cwd);
        let mut args = vec![OsString::from("--host"), directory, launch.program.into()];
        args.extend(launch.args);
        Launch {
            program: host.spawn.clone(),
            args,
            cwd: launch.cwd,
        }
    }

    /// `name` on `PATH` (a name with a slash is taken as it is). Windows finds a program
    /// named without an extension by trying the extensions programs have there.
    fn find(&self, name: &str) -> Option<PathBuf> {
        if let Some(host) = &self.host {
            return host.find(name);
        }
        let on_windows = self.system == System::Windows;
        let candidates = |path: PathBuf| {
            if on_windows {
                windows::candidates(path)
            } else {
                vec![path]
            }
        };
        if name.contains('/') || (on_windows && name.contains('\\')) {
            return candidates(PathBuf::from(name))
                .into_iter()
                .find(|candidate| is_executable(candidate));
        }
        let paths = self.path.as_ref()?;
        std::env::split_paths(paths)
            .flat_map(|dir| candidates(dir.join(name)))
            .find(|candidate| is_executable(candidate))
    }

    fn desktops(&self) -> impl Iterator<Item = &str> {
        self.desktop.as_deref().unwrap_or_default().split(':')
    }

    /// Whether a D-Bus service implements `org.freedesktop.FileManager1` (Dolphin,
    /// Nautilus, Nemo…), which can show a file selected in its folder.
    fn has_file_manager_service(&self) -> bool {
        if let Some(host) = &self.host {
            const SCRIPT: &str = r#"IFS=:; for dir in ${XDG_DATA_DIRS:-/usr/local/share:/usr/share}; do for f in "$dir"/dbus-1/services/*FileManager1.service; do [ -e "$f" ] && exit 0; done; done; exit 1"#;
            return host.output(&["/bin/sh", "-c", SCRIPT]).is_some();
        }
        let dirs = self
            .data_dirs
            .clone()
            .filter(|dirs| !dirs.is_empty())
            .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
        std::env::split_paths(&dirs).any(|dir| {
            std::fs::read_dir(dir.join("dbus-1/services")).is_ok_and(|entries| {
                entries.flatten().any(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .ends_with("FileManager1.service")
                })
            })
        })
    }
}

fn is_executable(path: &Path) -> bool {
    path.metadata().is_ok_and(|meta| {
        // Windows has no executable bit; being a file is enough there.
        meta.is_file() && (cfg!(windows) || gitweft_platform::fs::is_executable(&meta))
    })
}

/// The program `word` names, to be matched against the ones Gitweft knows: its file name,
/// on Windows in lower case and without `.exe`, as Windows reads names.
fn program_name(word: &str, system: System) -> String {
    if system == System::Windows {
        let name = word.rsplit(['/', '\\']).next().unwrap_or(word);
        let name = name.to_ascii_lowercase();
        return name.strip_suffix(".exe").unwrap_or(&name).to_owned();
    }
    Path::new(word)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Splits a command line into words as a shell would, without running one: spaces
/// separate words, `'…'` and `"…"` group them, a backslash escapes the next character
/// (outside single quotes). The same on every system, Windows included.
pub fn split_command(text: &str) -> Result<Vec<String>, LaunchError> {
    let bad = || LaunchError::BadCommand(text.to_owned());
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                loop {
                    match chars.next().ok_or_else(bad)? {
                        '\'' => break,
                        c => word.push(c),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next().ok_or_else(bad)? {
                        '"' => break,
                        '\\' => word.push(chars.next().ok_or_else(bad)?),
                        c => word.push(c),
                    }
                }
            }
            '\\' => {
                in_word = true;
                word.push(chars.next().ok_or_else(bad)?);
            }
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    if in_word {
        words.push(word);
    }
    if words.is_empty() {
        return Err(bad());
    }
    Ok(words)
}

/// `word` with each `{path}` replaced by `path`, and whether there was one.
fn expand(word: &str, path: &Path) -> (OsString, bool) {
    const PLACEHOLDER: &str = "{path}";
    if !word.contains(PLACEHOLDER) {
        return (word.into(), false);
    }
    let mut bytes = Vec::new();
    let mut parts = word.split(PLACEHOLDER).peekable();
    while let Some(part) = parts.next() {
        bytes.extend_from_slice(part.as_bytes());
        if parts.peek().is_some() {
            bytes.extend_from_slice(path.as_os_str().as_encoded_bytes());
        }
    }
    (os_string_from_vec(bytes), true)
}

/// A command setting run for `path`: its words with `{path}` expanded; with no `{path}`,
/// `path` is added at the end when `append` is set.
fn from_setting(
    setting: &str,
    path: &Path,
    append: bool,
    cwd: &Path,
    env: &Environment,
) -> Result<Launch, LaunchError> {
    let words = split_command(setting)?;
    let program = env
        .find(&words[0])
        .ok_or_else(|| LaunchError::NotFound(words[0].clone()))?;
    let mut used = false;
    let mut args: Vec<OsString> = words[1..]
        .iter()
        .map(|word| {
            let (arg, had) = expand(word, path);
            used |= had;
            arg
        })
        .collect();
    if append && !used {
        args.push(path.into());
    }
    Ok(Launch {
        program,
        args,
        cwd: cwd.to_owned(),
    })
}

/// The arguments that open a known terminal in `dir` (others start in their working
/// folder, which is `dir` too).
fn terminal_args(name: &str, dir: &Path, system: System) -> Vec<OsString> {
    if system == System::Windows && name == windows::TERMINAL {
        return windows::terminal_args(dir);
    }
    let flag = |flag: &str| {
        let mut arg = OsString::from(flag);
        arg.push(dir.as_os_str());
        vec![arg]
    };
    match name {
        "konsole" => vec!["--workdir".into(), dir.into()],
        "ptyxis" | "kgx" | "gnome-terminal" | "xfce4-terminal" | "mate-terminal" | "tilix"
        | "foot" => flag("--working-directory="),
        "alacritty" => vec!["--working-directory".into(), dir.into()],
        "kitty" => vec!["--directory".into(), dir.into()],
        "wezterm" => vec!["start".into(), "--cwd".into(), dir.into()],
        _ => Vec::new(),
    }
}

/// The terminal used when none is set: `$TERMINAL`, the desktop's own, then the first of
/// [`TERMINALS`] installed. On macOS it's Terminal; on Windows Windows Terminal, else cmd.
/// Its name, for the settings page.
pub fn default_terminal(env: &Environment) -> Option<String> {
    match env.system {
        System::MacOs => return Some(macos::TERMINAL.to_owned()),
        System::Windows => return windows::default_terminal(env),
        System::Linux => {}
    }
    let from_env = env
        .terminal
        .as_ref()
        .and_then(|value| split_command(&value.to_string_lossy()).ok())
        .map(|words| words[0].clone())
        .filter(|name| env.find(name).is_some());
    let desktop = env.desktops().find_map(|desktop| {
        let own: &[&str] = match desktop {
            "KDE" => &["konsole"],
            "GNOME" => &["ptyxis", "kgx", "gnome-terminal"],
            "XFCE" => &["xfce4-terminal"],
            "MATE" => &["mate-terminal"],
            _ => &[],
        };
        own.iter().find(|name| env.find(name).is_some())
    });
    from_env.or_else(|| {
        desktop
            .or_else(|| TERMINALS.iter().find(|name| env.find(name).is_some()))
            .map(|name| (*name).to_owned())
    })
}

/// A terminal in `dir`. `setting` is a command (`{path}` is `dir`; without it the terminal
/// just starts there); a known terminal's bare name gets its own folder option. Empty:
/// [`default_terminal`].
pub fn terminal(setting: &str, dir: &Path, env: &Environment) -> Result<Launch, LaunchError> {
    let setting = setting.trim();
    let command = if setting.is_empty() {
        default_terminal(env).ok_or(LaunchError::NoTerminal)?
    } else {
        setting.to_owned()
    };
    let words = split_command(&command)?;
    let name = program_name(&words[0], env.system);
    if words.len() == 1 && env.system == System::MacOs && macos::is_terminal(&name) {
        return macos::terminal(dir, env);
    }
    let mut launch = from_setting(&command, dir, false, dir, env)?;
    if words.len() == 1 {
        launch.args = terminal_args(&name, dir, env.system);
    }
    as_terminal(launch, &name, env)
}

/// A terminal (`name`) as it's started: on the host from a Flatpak; on Windows through a
/// console of its own (see [`windows::in_console`]), unless it's Windows Terminal.
fn as_terminal(launch: Launch, name: &str, env: &Environment) -> Result<Launch, LaunchError> {
    match env.system {
        System::Windows if name != windows::TERMINAL => windows::in_console(launch, env),
        _ => Ok(env.on_host(launch)),
    }
}

/// Where a terminal command of one's own takes the command to run: a word of its own
/// is replaced by the command's arguments, a part of a word by the command as one quoted
/// line (`sh -c '{command}'`).
const COMMAND_PLACEHOLDER: &str = "{command}";

/// `words` as one shell command line, each quoted, for a terminal that takes the command as
/// a single string (Tilix's `-e`, `{command}` inside a word). `None` when a word isn't UTF-8.
fn shell_line(words: &[OsString]) -> Option<String> {
    let quoted: Option<Vec<String>> = words
        .iter()
        .map(|word| Some(format!("'{}'", word.to_str()?.replace('\'', r"'\''"))))
        .collect();
    Some(quoted?.join(" "))
}

/// A terminal in `dir` that runs `command` (a program and its arguments) and closes when it
/// ends. A terminal Gitweft knows, set or found by its bare name, is told what to run in
/// its own way; a command setting of the user's own says where with `{command}` (without
/// it: [`LaunchError::NoCommandForm`]). Terminal on macOS is given it as a line for the
/// user's shell; on Windows the command gets a console of its own, or a Windows Terminal tab.
pub fn terminal_running(
    setting: &str,
    dir: &Path,
    command: &[OsString],
    env: &Environment,
) -> Result<Launch, LaunchError> {
    let setting = setting.trim();
    let terminal = if setting.is_empty() {
        default_terminal(env).ok_or(LaunchError::NoTerminal)?
    } else {
        setting.to_owned()
    };
    let words = split_command(&terminal)?;
    if words.iter().any(|word| word.contains(COMMAND_PLACEHOLDER)) {
        let program = env
            .find(&words[0])
            .ok_or_else(|| LaunchError::NotFound(words[0].clone()))?;
        let mut args = Vec::new();
        for word in &words[1..] {
            if word == COMMAND_PLACEHOLDER {
                args.extend(command.iter().cloned());
            } else if word.contains(COMMAND_PLACEHOLDER) {
                let line = shell_line(command)
                    .ok_or_else(|| LaunchError::NoCommandForm(terminal.clone()))?;
                args.push(expand(&word.replace(COMMAND_PLACEHOLDER, &line), dir).0);
            } else {
                args.push(expand(word, dir).0);
            }
        }
        let launch = Launch {
            program,
            args,
            cwd: dir.to_owned(),
        };
        return as_terminal(launch, &program_name(&words[0], env.system), env);
    }
    if words.len() != 1 {
        return Err(LaunchError::NoCommandForm(terminal));
    }
    let name = program_name(&words[0], env.system);
    match env.system {
        System::MacOs if macos::is_terminal(&name) => {
            return macos::terminal_running(dir, command, env);
        }
        System::Windows => {
            if let Some(launch) = windows::terminal_running(&name, &terminal, dir, command, env)? {
                return Ok(launch);
            }
        }
        _ => {}
    }
    // What comes before the command: `-e` takes the rest of the line in these, `--` ends
    // the terminal's own options in those (GNOME Console's `-- COMMAND` since 3.x); kitty
    // and foot take it as they are; Tilix's `-e` takes one string, which it splits as a
    // shell would.
    let mut launch = from_setting(&terminal, dir, false, dir, env)?;
    launch.args = terminal_args(&name, dir, env.system);
    match name.as_str() {
        "konsole" | "alacritty" | "xterm" => launch.args.push("-e".into()),
        "gnome-terminal" | "ptyxis" | "kgx" | "wezterm" => launch.args.push("--".into()),
        "xfce4-terminal" | "mate-terminal" => launch.args.push("-x".into()),
        "kitty" | "foot" => {}
        "tilix" => {
            let line = shell_line(command).ok_or(LaunchError::NoCommandForm(name.clone()))?;
            launch.args.extend(["-e".into(), line.into()]);
            return as_terminal(launch, &name, env);
        }
        _ => return Err(LaunchError::NoCommandForm(name)),
    }
    launch.args.extend(command.iter().cloned());
    as_terminal(launch, &name, env)
}

/// The file managers that select a file they're given, by desktop file (without
/// `.desktop`): the program and its options before the file.
fn selecting_file_manager(desktop_id: &str) -> Option<(&'static str, &'static [&'static str])> {
    let id = desktop_id.strip_suffix(".desktop").unwrap_or(desktop_id);
    match id {
        "org.kde.dolphin" | "dolphin" => Some(("dolphin", &["--select"])),
        "org.gnome.Nautilus" | "nautilus" => Some(("nautilus", &["--select"])),
        "nemo" => Some(("nemo", &[])),
        _ => None,
    }
}

/// The file manager showing `path`: a folder opened, a file selected in its folder.
/// `setting` is a command (`{path}` is `path`, else it's added at the end). Empty: the
/// default folder handler when it's one that selects files (Dolphin, Nautilus, Nemo), else
/// the desktop's file manager over D-Bus (several may offer that, and D-Bus picks one), else
/// `xdg-open` on the folder. The Finder on macOS, Explorer on Windows.
pub fn file_manager(
    setting: &str,
    path: &Path,
    is_dir: bool,
    env: &Environment,
) -> Result<Launch, LaunchError> {
    file_manager_here(setting, path, is_dir, env).map(|launch| env.on_host(launch))
}

fn file_manager_here(
    setting: &str,
    path: &Path,
    is_dir: bool,
    env: &Environment,
) -> Result<Launch, LaunchError> {
    let folder = if is_dir {
        path
    } else {
        path.parent().unwrap_or(path)
    };
    let setting = setting.trim();
    if !setting.is_empty() {
        return from_setting(setting, path, true, folder, env);
    }
    match env.system {
        System::MacOs => return macos::file_manager(path, is_dir, folder, env),
        System::Windows => return windows::file_manager(path, is_dir, folder, env),
        System::Linux => {}
    }
    let handler = (!is_dir).then(|| env.directory_handler()).flatten();
    if let Some((name, options)) = handler.as_deref().and_then(selecting_file_manager)
        && let Some(program) = env.find(name)
    {
        let mut args: Vec<OsString> = options.iter().map(OsString::from).collect();
        args.push(path.into());
        return Ok(Launch {
            program,
            args,
            cwd: folder.to_owned(),
        });
    }
    if !is_dir
        && env.has_file_manager_service()
        && let Some(program) = env.find("dbus-send")
    {
        let mut item = OsString::from("array:string:");
        item.push(file_uri(path));
        return Ok(Launch {
            program,
            args: vec![
                "--session".into(),
                "--print-reply".into(),
                "--type=method_call".into(),
                "--dest=org.freedesktop.FileManager1".into(),
                "/org/freedesktop/FileManager1".into(),
                "org.freedesktop.FileManager1.ShowItems".into(),
                item,
                "string:".into(),
            ],
            cwd: folder.to_owned(),
        });
    }
    xdg_open(folder, folder, env)
}

/// An editor with `file` open, started in `workdir`. `setting` is a command (`{path}` is
/// the file, else it's added at the end). Empty: the file's default application; on macOS
/// the default text editor, on Windows Notepad.
pub fn editor(
    setting: &str,
    file: &Path,
    workdir: &Path,
    env: &Environment,
) -> Result<Launch, LaunchError> {
    let setting = setting.trim();
    let launch = if setting.is_empty() {
        match env.system {
            System::Linux => xdg_open(file, workdir, env)?,
            System::MacOs => macos::editor(file, workdir, env)?,
            System::Windows => windows::editor(file, workdir, env)?,
        }
    } else {
        from_setting(setting, file, true, workdir, env)?
    };
    Ok(env.on_host(launch))
}

fn xdg_open(target: &Path, cwd: &Path, env: &Environment) -> Result<Launch, LaunchError> {
    let program = env
        .find("xdg-open")
        .ok_or_else(|| LaunchError::NotFound("xdg-open".into()))?;
    Ok(Launch {
        program,
        args: vec![target.into()],
        cwd: cwd.to_owned(),
    })
}

/// `file://` and the path, percent-encoded (every byte but unreserved ones and `/`), so a
/// comma or quote can't split a `dbus-send` argument.
fn file_uri(path: &Path) -> String {
    let mut uri = String::from("file://");
    for &byte in path.as_os_str().as_encoded_bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~/".contains(&byte) {
            uri.push(char::from(byte));
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    uri
}

/// Starts `launch` on its own (detached from us, no terminal input) and returns once
/// it runs. A program that exits with an error within a moment (a bad option, `xdg-open`
/// finding nothing to open with) reports its last message; Explorer, whose exit code means
/// nothing, never does. Blocks for up to 1.5 s: run it off the UI thread.
pub fn launch(launch: &Launch) -> Result<(), LaunchError> {
    let name = launch.program.file_name().map_or_else(
        || launch.program.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    );
    #[allow(clippy::disallowed_methods)] // The user's own programs, never git (see module docs).
    let mut command = Command::new(&launch.program);
    #[cfg(not(windows))]
    command.args(&launch.args);
    #[cfg(windows)]
    windows::add_args(&mut command, launch);
    command
        .current_dir(&launch.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let any_exit =
        System::current() == System::Windows && windows::exit_code_means_nothing(&launch.program);
    let mut child =
        gitweft_platform::process::spawn_detached(&mut command).map_err(|err| {
            match err.kind() {
                std::io::ErrorKind::NotFound => LaunchError::NotFound(name.clone()),
                _ => LaunchError::Failed {
                    program: name.clone(),
                    message: err.to_string(),
                },
            }
        })?;
    let mut stderr = child.stderr.take().expect("stderr is piped");
    // Read on a thread of its own: a chatty program must not block on a full pipe.
    let reader = std::thread::spawn(move || {
        let mut text = Vec::new();
        let _ = stderr.read_to_end(&mut text);
        text
    });
    let started = Instant::now();
    while started.elapsed() < QUICK_FAILURE {
        match child.try_wait() {
            Ok(Some(status)) if status.success() || any_exit => return Ok(()),
            Ok(Some(status)) => {
                let text = reader.join().unwrap_or_default();
                let text = String::from_utf8_lossy(&text);
                let message = text
                    .lines()
                    .rev()
                    .find(|line| !line.trim().is_empty())
                    .map_or_else(|| status.to_string(), |line| line.trim().to_owned());
                return Err(LaunchError::Failed {
                    program: name,
                    message,
                });
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(_) => break,
        }
    }
    // Still running: collect it when it ends, so it doesn't stay a zombie.
    std::thread::spawn(move || {
        let _ = reader.join();
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A folder of fake programs to be `PATH`, and an environment using it, on Linux.
    pub(super) fn programs(names: &[&str]) -> (tempfile::TempDir, Environment) {
        let dir = tempfile::tempdir().unwrap();
        for name in names {
            let path = dir.path().join(name);
            std::fs::write(&path, "#!/bin/sh\n").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        let env = Environment {
            system: System::Linux,
            path: Some(dir.path().as_os_str().to_owned()),
            data_dirs: Some(dir.path().join("share").into()),
            directory_handler: Some(None),
            ..Environment::default()
        };
        (dir, env)
    }

    #[cfg(unix)]
    #[test]
    fn a_known_terminal_is_told_to_run_a_command() {
        let (_dir, env) = programs(&[
            "konsole",
            "gnome-terminal",
            "kitty",
            "xterm",
            "tilix",
            "kgx",
            "myterm",
        ]);
        let dir = Path::new("/repo");
        let command: Vec<OsString> = ["git", "mergetool", "--", "a b.txt"]
            .iter()
            .map(OsString::from)
            .collect();
        let run = |setting: &str| terminal_running(setting, dir, &command, &env);
        assert_eq!(
            args(&run("konsole").unwrap()),
            [
                "--workdir",
                "/repo",
                "-e",
                "git",
                "mergetool",
                "--",
                "a b.txt"
            ]
        );
        assert_eq!(
            args(&run("gnome-terminal").unwrap()),
            [
                "--working-directory=/repo",
                "--",
                "git",
                "mergetool",
                "--",
                "a b.txt"
            ]
        );
        assert_eq!(
            args(&run("kitty").unwrap()),
            ["--directory", "/repo", "git", "mergetool", "--", "a b.txt"]
        );
        let xterm = run("xterm").unwrap();
        assert_eq!(args(&xterm), ["-e", "git", "mergetool", "--", "a b.txt"]);
        assert_eq!(xterm.cwd, dir);
        // Tilix takes the command as one line it splits itself, quotes kept.
        assert_eq!(
            args(&run("tilix").unwrap()),
            [
                "--working-directory=/repo",
                "-e",
                "'git' 'mergetool' '--' 'a b.txt'"
            ]
        );
        assert_eq!(
            args(&run("kgx").unwrap()),
            [
                "--working-directory=/repo",
                "--",
                "git",
                "mergetool",
                "--",
                "a b.txt"
            ]
        );
        // A command of one's own: `{command}` as a word takes the arguments, inside a word
        // the quoted line; `{path}` is the folder.
        assert_eq!(
            args(&run("myterm --dir {path} --run {command}").unwrap()),
            [
                "--dir",
                "/repo",
                "--run",
                "git",
                "mergetool",
                "--",
                "a b.txt"
            ]
        );
        assert_eq!(
            args(&run("myterm -c 'exec {command}'").unwrap()),
            ["-c", "exec 'git' 'mergetool' '--' 'a b.txt'"]
        );
        // A quote inside an argument survives the line.
        assert_eq!(
            shell_line(&["it's".into(), "a b".into()]).as_deref(),
            Some(r"'it'\''s' 'a b'")
        );
        // Without `{command}`, a command of one's own can't be told what to run.
        assert_eq!(
            run("konsole --profile Work"),
            Err(LaunchError::NoCommandForm("konsole --profile Work".into()))
        );
        // None set: the one found, as for a plain terminal.
        assert_eq!(name(&run("").unwrap()), "konsole");
    }

    pub(super) fn args(launch: &Launch) -> Vec<String> {
        launch
            .args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    pub(super) fn name(launch: &Launch) -> String {
        launch
            .program
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }

    #[test]
    fn commands_split_like_a_shell() {
        let split = |text: &str| split_command(text).unwrap();
        assert_eq!(split("code -g {path}"), ["code", "-g", "{path}"]);
        assert_eq!(
            split("  a   'b c' \"d \\\" e\" f\\ g "),
            ["a", "b c", "d \" e", "f g"]
        );
        assert_eq!(split("x ''"), ["x", ""]);
        for bad in ["", "   ", "a 'b", "a \"b", "a \\"] {
            assert!(
                matches!(split_command(bad), Err(LaunchError::BadCommand(_))),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn the_placeholder_expands_inside_words() {
        let path = Path::new("/r/a b");
        assert_eq!(expand("--dir={path}", path), ("--dir=/r/a b".into(), true));
        assert_eq!(
            expand("{path}:{path}", path).0,
            OsString::from("/r/a b:/r/a b")
        );
        assert_eq!(expand("-n", path), ("-n".into(), false));
    }

    #[cfg(unix)]
    #[test]
    fn a_path_that_is_not_utf8_expands_as_it_is() {
        let odd = gitweft_platform::bytes::path_from_bytes(b"/caf\xe9");
        assert_eq!(expand("{path}", &odd).0.as_encoded_bytes(), b"/caf\xe9");
        assert_eq!(file_uri(&odd), "file:///caf%E9");
    }

    #[cfg(unix)]
    #[test]
    fn terminals_are_found_and_opened_in_the_folder() {
        let dir = Path::new("/repo");
        let (_bin, mut env) = programs(&["xterm", "ptyxis", "konsole", "kitty"]);
        // The first known one when the desktop has none of its own.
        let launch = terminal("", dir, &env).unwrap();
        assert_eq!(
            (name(&launch), args(&launch)),
            ("konsole".into(), vec!["--workdir".into(), "/repo".into()])
        );
        assert_eq!(launch.cwd, dir);
        // GNOME's own comes first there.
        env.desktop = Some("ubuntu:GNOME".into());
        let launch = terminal("", dir, &env).unwrap();
        assert_eq!(name(&launch), "ptyxis");
        assert_eq!(args(&launch), ["--working-directory=/repo"]);
        // $TERMINAL wins, when installed.
        env.terminal = Some("kitty".into());
        assert_eq!(default_terminal(&env).as_deref(), Some("kitty"));
        let launch = terminal("", dir, &env).unwrap();
        assert_eq!(args(&launch), ["--directory", "/repo"]);
        env.terminal = Some("missing-term".into());
        assert_eq!(default_terminal(&env).as_deref(), Some("ptyxis"));
    }

    #[cfg(unix)]
    #[test]
    fn a_terminal_setting_is_used_as_written() {
        let dir = Path::new("/repo");
        let (_bin, env) = programs(&["konsole", "myterm"]);
        // A known name alone gets its folder option.
        assert_eq!(
            args(&terminal(" konsole ", dir, &env).unwrap()),
            ["--workdir", "/repo"]
        );
        // Anything else is taken as written: it starts in the folder.
        let launch = terminal("myterm -e fish", dir, &env).unwrap();
        assert_eq!(
            (name(&launch), args(&launch)),
            ("myterm".into(), vec!["-e".into(), "fish".into()])
        );
        assert_eq!(launch.cwd, dir);
        assert_eq!(
            args(&terminal("konsole --workdir {path} --new-tab", dir, &env).unwrap()),
            ["--workdir", "/repo", "--new-tab"]
        );
        assert_eq!(
            terminal("nothere", dir, &env),
            Err(LaunchError::NotFound("nothere".into()))
        );
        let (_bin, empty) = programs(&[]);
        assert_eq!(terminal("", dir, &empty), Err(LaunchError::NoTerminal));
    }

    #[cfg(unix)]
    #[test]
    fn the_file_manager_selects_a_file_when_the_desktop_can() {
        let (bin, env) = programs(&["xdg-open", "dbus-send", "dolphin"]);
        let file = Path::new("/r/a b,c.txt");
        // No FileManager1 service: the folder, with xdg-open.
        let launch = file_manager("", file, false, &env).unwrap();
        assert_eq!(
            (name(&launch), args(&launch)),
            ("xdg-open".into(), vec!["/r".into()])
        );
        let services = bin.path().join("share/dbus-1/services");
        std::fs::create_dir_all(&services).unwrap();
        std::fs::write(services.join("org.kde.dolphin.FileManager1.service"), "").unwrap();
        let launch = file_manager("", file, false, &env).unwrap();
        assert_eq!(name(&launch), "dbus-send");
        assert!(args(&launch).contains(&"array:string:file:///r/a%20b%2Cc.txt".to_owned()));
        assert_eq!(launch.cwd, Path::new("/r"));
        // A folder is opened.
        let launch = file_manager("", Path::new("/r"), true, &env).unwrap();
        assert_eq!(
            (name(&launch), args(&launch)),
            ("xdg-open".into(), vec!["/r".into()])
        );
        // A setting: the path at the end, or where {path} is.
        assert_eq!(
            args(&file_manager("dolphin --select", file, false, &env).unwrap()),
            ["--select", "/r/a b,c.txt"]
        );
        assert_eq!(
            args(&file_manager("dolphin {path} --new-window", file, false, &env).unwrap()),
            ["/r/a b,c.txt", "--new-window"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_default_folder_handler_selects_the_file_itself() {
        let (bin, mut env) = programs(&["xdg-open", "dbus-send", "dolphin"]);
        let services = bin.path().join("share/dbus-1/services");
        std::fs::create_dir_all(&services).unwrap();
        std::fs::write(services.join("org.freedesktop.FileManager1.service"), "").unwrap();
        let file = Path::new("/r/code.rs");
        // Dolphin by default, even though another file manager answers on D-Bus.
        env.directory_handler = Some(Some("org.kde.dolphin.desktop".into()));
        let launch = file_manager("", file, false, &env).unwrap();
        assert_eq!(
            (name(&launch), args(&launch)),
            (
                "dolphin".into(),
                vec!["--select".into(), "/r/code.rs".into()]
            )
        );
        assert_eq!(launch.cwd, Path::new("/r"));
        // A folder is still just opened.
        assert_eq!(
            name(&file_manager("", Path::new("/r"), true, &env).unwrap()),
            "xdg-open"
        );
        // Nautilus by default but not installed, or one that can't select: D-Bus.
        env.directory_handler = Some(Some("org.gnome.Nautilus.desktop".into()));
        assert_eq!(
            name(&file_manager("", file, false, &env).unwrap()),
            "dbus-send"
        );
        env.directory_handler = Some(Some("pcmanfm.desktop".into()));
        assert_eq!(
            name(&file_manager("", file, false, &env).unwrap()),
            "dbus-send"
        );
    }

    #[cfg(unix)]
    #[test]
    fn editors_open_the_file() {
        let (_bin, env) = programs(&["xdg-open", "code"]);
        let (file, workdir) = (Path::new("/r/src/main.rs"), Path::new("/r"));
        let launch = editor("", file, workdir, &env).unwrap();
        assert_eq!(
            (name(&launch), args(&launch)),
            ("xdg-open".into(), vec!["/r/src/main.rs".into()])
        );
        assert_eq!(launch.cwd, workdir);
        assert_eq!(
            args(&editor("code -r", file, workdir, &env).unwrap()),
            ["-r", "/r/src/main.rs"]
        );
        assert_eq!(
            args(&editor("code -g {path}:1", file, workdir, &env).unwrap()),
            ["-g", "/r/src/main.rs:1"]
        );
        let (_bin, bare) = programs(&[]);
        assert_eq!(
            editor("", file, workdir, &bare),
            Err(LaunchError::NotFound("xdg-open".into()))
        );
    }

    /// A stand-in for `flatpak-spawn` whose "host" has only `names` (fake programs) on its
    /// `PATH` and their `share` folder as `XDG_DATA_DIRS`, and an environment in a sandbox.
    #[cfg(unix)]
    fn host(names: &[&str]) -> (tempfile::TempDir, Environment) {
        let (dir, _) = programs(names);
        let spawn = dir.path().join("spawn-stub");
        let bin = dir.path().display();
        let script = format!(
            "#!/bin/sh\n[ \"$1\" = --host ] || exit 64\nshift\n\
             case \"$1\" in --directory=*) cd \"${{1#--directory=}}\" || exit 65; shift;; esac\n\
             export PATH='{bin}' XDG_DATA_DIRS='{bin}/share'\nexec \"$@\"\n"
        );
        write_program(&spawn, &script);
        let env = Environment {
            system: System::Linux,
            host: Some(Host::new(spawn, &[])),
            ..Environment::default()
        };
        (dir, env)
    }

    /// Writes a script and waits until it can be run: another test thread forking while it
    /// was written keeps it open in that child until the child execs, and running it fails
    /// with ETXTBSY meanwhile ("Text file busy"). Once it has run, nothing holds it open.
    #[cfg(unix)]
    fn write_program(path: &Path, script: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, script).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        for _ in 0..200 {
            #[allow(clippy::disallowed_methods)] // A test's stand-in program, never git.
            let ran = Command::new(path)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            match ran {
                Err(err) if err.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                _ => return,
            }
        }
    }

    #[cfg(unix)]
    fn texts(launch: &Launch) -> (PathBuf, Vec<String>) {
        (launch.program.clone(), args(launch))
    }

    #[cfg(unix)]
    #[test]
    fn in_a_sandbox_programs_are_the_hosts() {
        let (bin, env) = host(&["konsole", "xdg-open"]);
        let spawn = bin.path().join("spawn-stub");
        let konsole = bin.path().join("konsole").display().to_string();
        let dir = Path::new("/repo");
        assert_eq!(
            texts(&terminal("", dir, &env).unwrap()),
            (
                spawn.clone(),
                vec![
                    "--host".into(),
                    "--directory=/repo".into(),
                    konsole,
                    "--workdir".into(),
                    "/repo".into()
                ]
            )
        );
        // Only what the host has: this machine's own programs don't count.
        assert_eq!(
            terminal("xterm", dir, &env),
            Err(LaunchError::NotFound("xterm".into()))
        );
        // A program not asked about at the start is looked up when it's needed.
        write_program(&bin.path().join("late-editor"), "#!/bin/sh\n");
        let launch = editor("late-editor", Path::new("/repo/a.rs"), dir, &env).unwrap();
        assert_eq!(
            args(&launch)[2],
            bin.path().join("late-editor").display().to_string()
        );
        assert_eq!(args(&launch)[3], "/repo/a.rs");
    }

    #[cfg(unix)]
    #[test]
    fn in_a_sandbox_the_desktop_is_asked_on_the_host() {
        let (bin, env) = host(&["xdg-open", "dbus-send", "dolphin"]);
        let file = Path::new("/r/code.rs");
        let at = |name: &str| bin.path().join(name).display().to_string();
        // xdg-mime there names Dolphin, which selects the file.
        write_program(
            &bin.path().join("xdg-mime"),
            "#!/bin/sh\necho org.kde.dolphin.desktop\n",
        );
        let launch = file_manager("", file, false, &env).unwrap();
        assert_eq!(
            args(&launch)[2..],
            [at("dolphin"), "--select".into(), "/r/code.rs".into()]
        );
        // Another default handler and no FileManager1 service there: the folder, in xdg-open.
        write_program(
            &bin.path().join("xdg-mime"),
            "#!/bin/sh\necho pcmanfm.desktop\n",
        );
        let launch = file_manager("", file, false, &env).unwrap();
        assert_eq!(args(&launch)[2..], [at("xdg-open"), "/r".into()]);
        let services = bin.path().join("share/dbus-1/services");
        std::fs::create_dir_all(&services).unwrap();
        std::fs::write(services.join("org.freedesktop.FileManager1.service"), "").unwrap();
        let launch = file_manager("", file, false, &env).unwrap();
        assert!(
            args(&launch)[2].ends_with("/dbus-send"),
            "{:?}",
            args(&launch)
        );
    }

    #[cfg(unix)]
    #[test]
    fn in_a_sandbox_launching_runs_on_the_host_in_the_folder() {
        let (bin, env) = host(&["mark"]);
        write_program(&bin.path().join("mark"), "#!/bin/sh\npwd > \"$1\"\n");
        let folder = tempfile::tempdir().unwrap();
        let out = bin.path().join("out.txt");
        let launch = env.on_host(Launch {
            program: bin.path().join("mark"),
            args: vec![out.clone().into()],
            cwd: folder.path().to_owned(),
        });
        assert_eq!(super::launch(&launch), Ok(()));
        let started = Instant::now();
        while !out.exists() && started.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(20));
        }
        let pwd = std::fs::read_to_string(&out).unwrap();
        assert_eq!(Path::new(pwd.trim()), folder.path());
    }

    #[test]
    fn uris_are_percent_encoded() {
        assert_eq!(file_uri(Path::new("/a/b-c_d.e~f")), "file:///a/b-c_d.e~f");
        assert_eq!(
            file_uri(Path::new("/x y/'q',\"")),
            "file:///x%20y/%27q%27%2C%22"
        );
    }

    #[cfg(unix)]
    #[test]
    fn launching_reports_quick_failures_only() {
        let sh = |script: &str| Launch {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
            cwd: std::env::temp_dir(),
        };
        assert_eq!(launch(&sh("exit 0")), Ok(()));
        assert_eq!(
            launch(&sh("echo first >&2; echo 'no such file' >&2; exit 3")),
            Err(LaunchError::Failed {
                program: "sh".into(),
                message: "no such file".into()
            })
        );
        // Still running after the wait: fine, and returns at once afterwards.
        let started = Instant::now();
        assert_eq!(launch(&sh("sleep 3")), Ok(()));
        assert!(started.elapsed() < Duration::from_millis(2500));
        let missing = Launch {
            program: "/nonexistent/prog".into(),
            args: Vec::new(),
            cwd: std::env::temp_dir(),
        };
        assert_eq!(launch(&missing), Err(LaunchError::NotFound("prog".into())));
    }
}
