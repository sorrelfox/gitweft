//! Runs the system `git` binary with a controlled environment, streamed stderr
//! progress and process-group cancellation.
//!
//! All calls are blocking; the job system runs them on worker threads.

use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use super::error::{GitError, GitErrorKind, classify};
use super::progress::{StderrEvent, StderrSplitter, parse_stderr_line};
use super::version::GitVersion;
use crate::activity::{ActivityEntry, ActivityLog, ActivityOutcome};
use crate::askpass;
use crate::cancel::CancelToken;

const POLL_INTERVAL: Duration = Duration::from_millis(20);
const KILL_GRACE: Duration = Duration::from_secs(2);

/// Receives a streaming command's stdout, chunk by chunk.
type StdoutSink<'a> = &'a mut dyn FnMut(&[u8]);

/// Handle to a verified `git` executable plus the environment every child gets.
#[derive(Debug, Clone)]
pub struct GitCli {
    program: PathBuf,
    version: GitVersion,
    env: Vec<(OsString, OsString)>,
    /// Where finished commands are recorded, if anywhere.
    activity: Option<ActivityLog>,
    /// Commands run on the host through this program (`flatpak-spawn`): see [`Self::on_host`].
    host: Option<PathBuf>,
}

/// Captured result of a successful git invocation.
#[derive(Debug, Clone)]
pub struct GitOutput {
    pub stdout: Vec<u8>,
    pub stderr: String,
    /// The exit code: 0, or one allowed with [`GitCommand::allow_exit_codes`].
    pub status: i32,
}

impl GitOutput {
    pub fn stdout_str(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.stdout)
    }
}

impl GitCli {
    /// Locates `git` (honouring `GITWEFT_GIT` as an override) and checks its version.
    pub fn discover() -> Result<Self, GitError> {
        let program = std::env::var_os("GITWEFT_GIT")
            .map(PathBuf::from)
            .or_else(|| find_in_path("git"))
            .ok_or_else(|| {
                GitError::new(GitErrorKind::GitMissing, "git", "git was not found in PATH")
            })?;
        Self::with_program(program)
    }

    pub fn with_program(program: impl Into<PathBuf>) -> Result<Self, GitError> {
        let program = program.into();
        #[allow(clippy::disallowed_methods)] // The runner is the one place allowed to spawn git.
        let output = Command::new(&program)
            .arg("version")
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .output()
            .map_err(|e| {
                GitError::new(
                    GitErrorKind::GitMissing,
                    program.display().to_string(),
                    e.to_string(),
                )
            })?;
        let text = String::from_utf8_lossy(&output.stdout);
        let version = GitVersion::parse(&text).ok_or_else(|| {
            GitError::new(
                GitErrorKind::GitMissing,
                "git version",
                format!("unexpected output: {text}"),
            )
        })?;
        if !version.is_supported() {
            return Err(GitError::new(
                GitErrorKind::GitTooOld,
                "git version",
                format!(
                    "git {version} is too old; Gitweft needs {} or newer",
                    GitVersion::MINIMUM
                ),
            ));
        }
        Ok(Self {
            program,
            version,
            env: base_env(),
            activity: None,
            host: None,
        })
    }

    /// This handle, running its commands on the host instead, through `spawn`
    /// (`flatpak-spawn`): in a Flatpak, git's diff and merge tools are the host's programs.
    /// The host's `git` must answer `git version`. The activity log and the environment
    /// carry over (handed to the host with `--env`), except the askpass wiring: its helper is
    /// this sandbox's program.
    pub fn on_host(&self, spawn: impl Into<PathBuf>) -> Result<Self, GitError> {
        let spawn = spawn.into();
        let missing = |why: String| GitError::new(GitErrorKind::GitMissing, "git version", why);
        #[allow(clippy::disallowed_methods)] // The runner is the one place allowed to spawn git.
        let output = Command::new(&spawn)
            .args(["--host", "--env=LC_ALL=C", "git", "version"])
            .stdin(Stdio::null())
            .output()
            .map_err(|e| missing(format!("{}: {e}", spawn.display())))?;
        let version = GitVersion::parse(&String::from_utf8_lossy(&output.stdout))
            .ok_or_else(|| missing(String::from_utf8_lossy(&output.stderr).trim().to_owned()))?;
        let sandbox_only = [
            askpass::ENV_SOCKET,
            askpass::ENV_TOKEN,
            "GIT_ASKPASS",
            "SSH_ASKPASS",
            "SSH_ASKPASS_REQUIRE",
        ];
        let env = self
            .env
            .iter()
            .filter(|(key, _)| !sandbox_only.iter().any(|name| key == name))
            .cloned()
            .collect();
        Ok(Self {
            program: "git".into(),
            version,
            env,
            activity: self.activity.clone(),
            host: Some(spawn),
        })
    }

    /// Records every command run through this handle (and its clones made afterwards).
    pub fn set_activity_log(&mut self, log: ActivityLog) {
        self.activity = Some(log);
    }

    pub fn version(&self) -> GitVersion {
        self.version
    }

    pub fn program(&self) -> &Path {
        &self.program
    }

    /// Adds or replaces an environment variable for every future command
    /// (used for askpass wiring and test isolation).
    pub fn set_env(&mut self, key: impl Into<OsString>, value: impl Into<OsString>) {
        let key = key.into();
        self.env.retain(|(k, _)| *k != key);
        self.env.push((key, value.into()));
    }

    /// The value this handle gives `key` in every command's environment, if it sets one
    /// (otherwise git inherits this process's value).
    pub fn env_var(&self, key: impl AsRef<OsStr>) -> Option<&OsStr> {
        let key = key.as_ref();
        self.env
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_os_str())
    }

    pub fn command(&self, cwd: impl Into<PathBuf>) -> GitCommand<'_> {
        GitCommand {
            cli: self,
            cwd: cwd.into(),
            args: Vec::new(),
            env: Vec::new(),
            stdin: None,
            read_only: false,
            non_interactive: false,
            progress_on_stdout: false,
            plain_stderr: false,
            allowed_exit_codes: Vec::new(),
        }
    }
}

/// Environment applied to every git child process.
fn base_env() -> Vec<(OsString, OsString)> {
    [
        // Never block on a terminal prompt; prompts go through GIT_ASKPASS.
        ("GIT_TERMINAL_PROMPT", "0"),
        ("GIT_PAGER", "cat"),
        ("PAGER", "cat"),
        // Flows that need a message editor override this explicitly.
        ("GIT_EDITOR", "true"),
        ("GIT_MERGE_AUTOEDIT", "no"),
        // Stable English messages for error classification.
        ("LC_MESSAGES", "C"),
    ]
    .into_iter()
    .map(|(k, v)| (k.into(), v.into()))
    .collect()
}

pub struct GitCommand<'a> {
    cli: &'a GitCli,
    cwd: PathBuf,
    args: Vec<OsString>,
    env: Vec<(OsString, OsString)>,
    stdin: Option<Vec<u8>>,
    read_only: bool,
    non_interactive: bool,
    progress_on_stdout: bool,
    plain_stderr: bool,
    allowed_exit_codes: Vec<i32>,
}

impl GitCommand<'_> {
    pub fn arg(mut self, arg: impl AsRef<OsStr>) -> Self {
        self.args.push(arg.as_ref().to_owned());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.args
            .extend(args.into_iter().map(|a| a.as_ref().to_owned()));
        self
    }

    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    pub fn stdin(mut self, bytes: impl Into<Vec<u8>>) -> Self {
        self.stdin = Some(bytes.into());
        self
    }

    /// Marks the command as a pure read, so it never takes optional locks
    /// (avoids fighting a terminal `git` over `index.lock`).
    pub fn read_only(mut self) -> Self {
        self.read_only = true;
        self
    }

    /// Makes every credential and ssh prompt fail at once instead of asking, for runs
    /// nobody is watching (a background fetch). Credential helpers that already know the
    /// secret still work; only prompts are refused. The askpass helper refuses by itself
    /// ([`crate::askpass::ENV_REFUSE`]); where git isn't wired to it, both askpass
    /// variables point at `false`, so neither the user's own askpass (ksshaskpass, …) nor
    /// ssh's terminal prompt can come up.
    pub fn non_interactive(mut self) -> Self {
        self.non_interactive = true;
        self
    }

    /// Also parses stdout for progress records: git-lfs writes its meter
    /// (`Downloading LFS objects:  50% (1/2), …\r`) there, not to stderr. The progress
    /// reaches the event callback; stdout is still returned whole. Ignored by
    /// [`Self::run_streaming`].
    pub fn progress_on_stdout(mut self) -> Self {
        self.progress_on_stdout = true;
        self
    }

    /// Hands every stderr record over as a [`StderrEvent::Line`], never parsing it as
    /// progress: for output that isn't git's own, such as a user's command under
    /// `git bisect run`, where `Tests: 5` is a line to show, not a meter.
    pub fn plain_stderr(mut self) -> Self {
        self.plain_stderr = true;
        self
    }

    /// Treats these non-zero exit codes as success: `git merge-tree` exits with 1 when the
    /// merge would conflict, which is an answer, not a failure. See [`GitOutput::status`].
    pub fn allow_exit_codes(mut self, codes: &[i32]) -> Self {
        self.allowed_exit_codes.extend_from_slice(codes);
        self
    }

    /// Command line for logs and error messages.
    pub fn display(&self) -> String {
        std::iter::once("git".to_owned())
            .chain(self.args.iter().map(|a| a.to_string_lossy().into_owned()))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Runs to completion, discarding progress.
    pub fn run(self, cancel: &CancelToken) -> Result<GitOutput, GitError> {
        self.run_with_events(cancel, |_| {})
    }

    /// Runs to completion, forwarding each stderr record (progress or message). The
    /// finished command goes into the activity log, if one is attached.
    pub fn run_with_events(
        self,
        cancel: &CancelToken,
        on_event: impl FnMut(StderrEvent),
    ) -> Result<GitOutput, GitError> {
        self.run_recorded(cancel, on_event, None)
    }

    /// Runs to completion, handing stdout to `on_stdout` in chunks as git writes it, so a
    /// long output (blame) can be shown while it arrives. Chunks may split lines anywhere,
    /// and the returned [`GitOutput::stdout`] is empty.
    pub fn run_streaming(
        self,
        cancel: &CancelToken,
        mut on_stdout: impl FnMut(&[u8]),
    ) -> Result<GitOutput, GitError> {
        let on_stdout: StdoutSink<'_> = &mut on_stdout;
        self.run_recorded(cancel, |_| {}, Some(on_stdout))
    }

    /// [`Self::run_streaming`] that also forwards each stderr record, like
    /// [`Self::run_with_events`]. Both callbacks run on this thread, one at a time; how
    /// stdout and stderr interleave is only as exact as the two pipes are read.
    pub fn run_streaming_with_events(
        self,
        cancel: &CancelToken,
        on_event: impl FnMut(StderrEvent),
        mut on_stdout: impl FnMut(&[u8]),
    ) -> Result<GitOutput, GitError> {
        let on_stdout: StdoutSink<'_> = &mut on_stdout;
        self.run_recorded(cancel, on_event, Some(on_stdout))
    }

    fn run_recorded(
        self,
        cancel: &CancelToken,
        on_event: impl FnMut(StderrEvent),
        on_stdout: Option<StdoutSink<'_>>,
    ) -> Result<GitOutput, GitError> {
        let record = self.cli.activity.clone().map(|log| {
            (
                log,
                self.display(),
                self.cwd.clone(),
                self.read_only,
                SystemTime::now(),
                Instant::now(),
            )
        });
        let result = self.execute(cancel, on_event, on_stdout);
        if let Some((log, command, cwd, read_only, started, clock)) = record {
            let outcome = match &result {
                Ok(_) => ActivityOutcome::Succeeded,
                Err(err) if err.kind == GitErrorKind::Cancelled => ActivityOutcome::Cancelled,
                Err(err) => ActivityOutcome::Failed {
                    kind: err.kind.clone(),
                    summary: err.summary(),
                },
            };
            log.record(ActivityEntry {
                command,
                cwd,
                started,
                duration: clock.elapsed(),
                read_only,
                outcome,
            });
        }
        result
    }

    fn execute(
        self,
        cancel: &CancelToken,
        mut on_event: impl FnMut(StderrEvent),
        mut on_stdout: Option<StdoutSink<'_>>,
    ) -> Result<GitOutput, GitError> {
        let display = self.display();
        let spawn_error =
            |e: std::io::Error| GitError::new(GitErrorKind::GitMissing, &display, e.to_string());

        // The variables set for this command, a later value of a key replacing an earlier one.
        let mut env: Vec<(OsString, OsString)> = Vec::new();
        let mut put = |key: &OsStr, value: &OsStr| {
            env.retain(|(k, _)| k != key);
            env.push((key.to_owned(), value.to_owned()));
        };
        for (key, value) in self.cli.env.iter().chain(&self.env) {
            put(key, value);
        }
        if self.read_only {
            put("GIT_OPTIONAL_LOCKS".as_ref(), "0".as_ref());
            // In a partial clone, a read never downloads what's missing by itself
            // (OFFLINE.md); git 2.44 and newer honour it.
            put("GIT_NO_LAZY_FETCH".as_ref(), "1".as_ref());
        }
        if self.non_interactive {
            let socket = OsStr::new(askpass::ENV_SOCKET);
            let wired = self
                .cli
                .env
                .iter()
                .chain(&self.env)
                .any(|(key, _)| key == socket);
            // `force`: ssh uses SSH_ASKPASS even with a terminal attached, never /dev/tty.
            put(askpass::ENV_REFUSE.as_ref(), "1".as_ref());
            put("SSH_ASKPASS_REQUIRE".as_ref(), "force".as_ref());
            if !wired {
                put("GIT_ASKPASS".as_ref(), "false".as_ref());
                put("SSH_ASKPASS".as_ref(), "false".as_ref());
            }
        }
        // LANGUAGE overrides LC_MESSAGES for gettext: unset unless the command sets it.
        let language = env.iter().any(|(key, _)| key == "LANGUAGE");

        #[allow(clippy::disallowed_methods)] // The runner is the one place allowed to spawn git.
        let mut cmd = match &self.cli.host {
            None => {
                let mut cmd = Command::new(&self.cli.program);
                if !language {
                    cmd.env_remove("LANGUAGE");
                }
                cmd.envs(env.iter().map(|(k, v)| (k, v)));
                cmd
            }
            // flatpak-spawn hands the host none of this process's environment, only `--env`'s;
            // `--watch-bus` ends the host's git when the spawner goes (cancelling kills the
            // spawner's group here, which the host's processes aren't in).
            Some(spawn) => {
                let mut cmd = Command::new(spawn);
                let mut directory = OsString::from("--directory=");
                directory.push(&self.cwd);
                cmd.args([OsStr::new("--host"), OsStr::new("--watch-bus"), &directory]);
                if !language {
                    cmd.arg("--env=LANGUAGE=");
                }
                for (key, value) in &env {
                    let mut arg = OsString::from("--env=");
                    arg.push(key);
                    arg.push("=");
                    arg.push(value);
                    cmd.arg(arg);
                }
                cmd.arg(&self.cli.program);
                cmd
            }
        };
        // `log.showSignature` would put a signature check on stdout before each signed
        // commit in `log`/`show` output, which is parsed; a read that wants the check asks
        // for it (`--show-signature`, `%G?`), which overrides this.
        cmd.args([
            "-c",
            "color.ui=false",
            "-c",
            "core.quotepath=off",
            "-c",
            "log.showSignature=false",
        ])
        .args(&self.args)
        .current_dir(&self.cwd)
        .stdin(if self.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Own process group, so cancellation also reaches ssh, hooks and helpers.
        .process_group(0);

        let mut child = cmd.spawn().map_err(spawn_error)?;

        let stdin_writer = match (self.stdin, child.stdin.take()) {
            (Some(bytes), Some(mut pipe)) => Some(thread::spawn(move || {
                // A broken pipe just means git stopped reading; its exit status tells the story.
                let _ = pipe.write_all(&bytes);
            })),
            _ => None,
        };

        // A streaming caller gets stdout in chunks through the loop below; otherwise it is
        // collected whole.
        let streaming = on_stdout.is_some();
        let progress_on_stdout = self.progress_on_stdout && !streaming;
        let (stdout_tx, stdout_rx) = mpsc::channel::<Vec<u8>>();
        let (progress_tx, progress_rx) = mpsc::channel::<StderrEvent>();
        let mut stdout_pipe = child.stdout.take().expect("stdout is piped");
        let stdout_reader = thread::spawn(move || {
            let mut buf = Vec::new();
            if !streaming && !progress_on_stdout {
                let _ = stdout_pipe.read_to_end(&mut buf);
                return buf;
            }
            let mut splitter = StderrSplitter::default();
            let forward_progress = |records: Vec<String>| {
                for record in records {
                    let event = parse_stderr_line(&record);
                    if matches!(event, StderrEvent::Progress(_)) {
                        let _ = progress_tx.send(event);
                    }
                }
            };
            let mut chunk = vec![0u8; 64 * 1024];
            loop {
                match stdout_pipe.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) if streaming => {
                        if stdout_tx.send(chunk[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                    Ok(n) => {
                        buf.extend_from_slice(&chunk[..n]);
                        forward_progress(splitter.feed(&chunk[..n]));
                    }
                }
            }
            forward_progress(splitter.finish().into_iter().collect());
            buf
        });

        let (tx, rx) = mpsc::channel::<String>();
        let mut stderr_pipe = child.stderr.take().expect("stderr is piped");
        let stderr_reader = thread::spawn(move || {
            let mut splitter = StderrSplitter::default();
            let mut chunk = [0u8; 8192];
            loop {
                match stderr_pipe.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        for record in splitter.feed(&chunk[..n]) {
                            let _ = tx.send(record);
                        }
                    }
                }
            }
            if let Some(record) = splitter.finish() {
                let _ = tx.send(record);
            }
        });

        let mut stderr_lines = Vec::new();
        let plain_stderr = self.plain_stderr;
        let mut handle = |record: String, on_event: &mut dyn FnMut(StderrEvent)| {
            let event = if plain_stderr {
                StderrEvent::Line(record)
            } else {
                parse_stderr_line(&record)
            };
            if let StderrEvent::Line(line) = &event {
                stderr_lines.push(line.clone());
            }
            on_event(event);
        };

        let status = loop {
            match rx.recv_timeout(POLL_INTERVAL) {
                Ok(record) => handle(record, &mut on_event),
                Err(mpsc::RecvTimeoutError::Timeout | mpsc::RecvTimeoutError::Disconnected) => {}
            }
            progress_rx.try_iter().for_each(&mut on_event);
            forward_stdout(&stdout_rx, &mut on_stdout);
            if cancel.is_cancelled() {
                kill_group(&mut child);
                break None;
            }
            if let Some(status) = child.try_wait().map_err(spawn_error)? {
                break Some(status);
            }
        };

        let _ = stderr_reader.join();
        for record in rx.try_iter() {
            handle(record, &mut on_event);
        }
        let stdout = stdout_reader.join().unwrap_or_default();
        progress_rx.try_iter().for_each(&mut on_event);
        if status.is_some() {
            forward_stdout(&stdout_rx, &mut on_stdout);
        }
        if let Some(writer) = stdin_writer {
            let _ = writer.join();
        }
        let stderr = stderr_lines.join("\n");

        let Some(status) = status else {
            return Err(GitError {
                kind: GitErrorKind::Cancelled,
                command: display,
                stderr,
                exit_code: None,
            });
        };
        let code = status.code().unwrap_or(-1);
        if status.success() || self.allowed_exit_codes.contains(&code) {
            return Ok(GitOutput {
                stdout,
                stderr,
                status: code,
            });
        }
        let stdout_text = String::from_utf8_lossy(&stdout);
        let mut kind = classify(&stderr, &stdout_text);
        // A local hook that fails speaks only in its own words: git adds nothing, or just
        // "failed to push some refs". A command that runs hooks, failing for no reason git
        // gave while such a hook is in place, failed in that hook.
        if kind == GitErrorKind::Other && has_blocking_hook(self.cli, &self.cwd, &self.args) {
            kind = GitErrorKind::HookFailed;
        }
        Err(GitError {
            kind,
            command: display,
            stderr,
            exit_code: status.code().or_else(|| status.signal().map(|s| 128 + s)),
        })
    }
}

/// Whether the subcommand in `args` runs a hook that can stop it and the repository at
/// `cwd` has one: an executable file where git looks (`git rev-parse --git-path hooks`,
/// which follows `core.hooksPath`). Asked only after a failure git didn't explain.
fn has_blocking_hook(cli: &GitCli, cwd: &std::path::Path, args: &[OsString]) -> bool {
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::PermissionsExt as _;
    let hooks: &[&str] = match args.first().map(|arg| arg.as_bytes()) {
        Some(b"commit") => &["pre-commit", "prepare-commit-msg", "commit-msg"],
        Some(b"merge") => &["pre-merge-commit", "prepare-commit-msg", "commit-msg"],
        Some(b"push") => &["pre-push"],
        Some(b"rebase") => &["pre-rebase"],
        Some(b"am") => &["applypatch-msg", "pre-applypatch"],
        _ => return false,
    };
    let Ok(output) = cli
        .command(cwd)
        .args(["rev-parse", "--git-path", "hooks"])
        .read_only()
        .run(&CancelToken::new())
    else {
        return false;
    };
    let listed = output.stdout.strip_suffix(b"\n").unwrap_or(&output.stdout);
    let dir = cwd.join(std::ffi::OsStr::from_bytes(listed));
    hooks.iter().any(|name| {
        std::fs::metadata(dir.join(name))
            .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
    })
}

/// Hands the stdout chunks read so far to a streaming caller.
fn forward_stdout(chunks: &mpsc::Receiver<Vec<u8>>, on_stdout: &mut Option<StdoutSink<'_>>) {
    if let Some(on_stdout) = on_stdout {
        for chunk in chunks.try_iter() {
            on_stdout(&chunk);
        }
    }
}

/// SIGTERM the whole process group, then SIGKILL it after a grace period, or as soon as git
/// itself has gone: a child that ignores SIGTERM (a hook, a credential helper) would keep
/// stderr open, and its reader waiting, forever (an earlier version sent the SIGKILL only to a
/// git that outlived the grace period). Git isn't reaped until then, so its pid, the group's
/// id, can't be anybody else's meanwhile.
fn kill_group(child: &mut Child) {
    let Ok(pid) = i32::try_from(child.id()) else {
        return;
    };
    // SAFETY: plain syscall on the process group we created with process_group(0).
    unsafe { libc::kill(-pid, libc::SIGTERM) };
    let deadline = Instant::now() + KILL_GRACE;
    while Instant::now() < deadline && !exited_unreaped(pid) {
        thread::sleep(POLL_INTERVAL);
    }
    // SAFETY: as above; the leader is at worst a zombie, so the group is still ours.
    unsafe { libc::kill(-pid, libc::SIGKILL) };
    let _ = child.wait();
}

/// Whether our child `pid` has exited, leaving it unreaped (`WNOWAIT`).
fn exited_unreaped(pid: i32) -> bool {
    let Ok(id) = libc::id_t::try_from(pid) else {
        return true;
    };
    // SAFETY: a zeroed siginfo_t is valid; waitid only reads our own child's state here.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let flags = libc::WEXITED | libc::WNOHANG | libc::WNOWAIT;
    // SAFETY: as above; `info` outlives the call.
    let found = unsafe { libc::waitid(libc::P_PID, id, &mut info, flags) };
    // SAFETY: si_pid is set by waitid (0 when the child is still running).
    found == 0 && unsafe { info.si_pid() } != 0
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(name))
            .find(|candidate| is_executable(candidate))
    })
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}
