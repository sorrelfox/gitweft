//! Credential prompts from git and ssh, relayed to the GUI.
//!
//! When git or ssh need a secret (an HTTPS password, a key passphrase, a host-key
//! confirmation), they run `$GIT_ASKPASS "<prompt>"` / `$SSH_ASKPASS "<prompt>"` and read the
//! answer from its stdout. Gitweft points both at its own binary. In helper mode, that
//! binary forwards the prompt over a private Unix socket to the running app
//! ([`AskpassServer`]), which asks the user and sends the answer back.
//!
//! Gitweft never stores what the user types; git's own credential helpers decide that.
//!
//! Wire protocol (one request per connection):
//! client → server: `MAGIC`, token, `\n`, prompt bytes, then shutdown(write)
//! server → client: `y` + answer, or `n` for "cancelled/refused"

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;

/// Set in git's environment; its presence puts the gitweft binary into helper mode.
pub const ENV_SOCKET: &str = "GITWEFT_ASKPASS_SOCKET";
pub const ENV_TOKEN: &str = "GITWEFT_ASKPASS_TOKEN";
/// Set (to `1`) for runs nobody is watching, such as a background fetch: the helper then
/// refuses every prompt at once, without contacting the app. Credential helpers that
/// already know the secret still answer, since git asks them before prompting.
pub const ENV_REFUSE: &str = "GITWEFT_ASKPASS_REFUSE";

const MAGIC: &[u8] = b"gitweft-askpass-1\n";
const MAX_REQUEST: u64 = 64 * 1024;

/// What kind of input a prompt wants, so the UI can pick the right widget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    /// Password, passphrase or PIN: masked input.
    Secret,
    /// ssh host-key confirmation: "Are you sure you want to continue connecting (yes/no/[fingerprint])?"
    Confirm,
    /// Anything else, e.g. "Username for 'https://host':".
    Text,
}

/// A prompt waiting for the user. Dropping it without answering counts as cancel.
#[derive(Debug)]
pub struct AskpassRequest {
    pub prompt: String,
    reply: mpsc::Sender<Option<String>>,
}

impl AskpassRequest {
    pub fn kind(&self) -> PromptKind {
        let lower = self.prompt.to_lowercase();
        if lower.contains("(yes/no") {
            PromptKind::Confirm
        } else if ["password", "passphrase", "pin for", "pin:"]
            .iter()
            .any(|needle| lower.contains(needle))
        {
            PromptKind::Secret
        } else {
            PromptKind::Text
        }
    }

    pub fn answer(self, value: String) {
        let _ = self.reply.send(Some(value));
    }

    pub fn cancel(self) {
        let _ = self.reply.send(None);
    }
}

/// Listens for helper connections for as long as it lives.
pub struct AskpassServer {
    socket_path: PathBuf,
    token: String,
    shutdown: Arc<AtomicBool>,
}

impl AskpassServer {
    /// Starts a server with its socket in `$XDG_RUNTIME_DIR/gitweft/`.
    pub fn start() -> io::Result<(Self, async_channel::Receiver<AskpassRequest>)> {
        let base = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        Self::start_in(&base.join("gitweft"))
    }

    /// Starts a server with its socket in `dir` (created with mode 0700).
    pub fn start_in(dir: &Path) -> io::Result<(Self, async_channel::Receiver<AskpassRequest>)> {
        static COUNTER: AtomicU32 = AtomicU32::new(0);

        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        let socket_path = dir.join(format!(
            "askpass-{}-{}.sock",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_file(&socket_path);
        let listener = UnixListener::bind(&socket_path)?;
        std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))?;

        let token = random_token()?;
        let shutdown = Arc::new(AtomicBool::new(false));
        let (tx, rx) = async_channel::unbounded();
        {
            let token = token.clone();
            let shutdown = Arc::clone(&shutdown);
            thread::Builder::new()
                .name("gitweft-askpass".into())
                .spawn(move || {
                    for stream in listener.incoming() {
                        if shutdown.load(Ordering::Relaxed) {
                            break;
                        }
                        let Ok(stream) = stream else { continue };
                        let (token, tx) = (token.clone(), tx.clone());
                        // Each prompt waits for the user, so never block the accept loop.
                        thread::spawn(move || {
                            let _ = serve(stream, &token, &tx);
                        });
                    }
                })?;
        }
        Ok((
            Self {
                socket_path,
                token,
                shutdown,
            },
            rx,
        ))
    }

    /// Environment that routes git's and ssh's prompts to this server through `helper`
    /// (the path of the gitweft executable).
    pub fn git_env(&self, helper: &Path) -> Vec<(OsString, OsString)> {
        vec![
            ("GIT_ASKPASS".into(), helper.into()),
            ("SSH_ASKPASS".into(), helper.into()),
            // Use SSH_ASKPASS even though a terminal may be attached.
            ("SSH_ASKPASS_REQUIRE".into(), "force".into()),
            (ENV_SOCKET.into(), self.socket_path.clone().into()),
            (ENV_TOKEN.into(), self.token.clone().into()),
        ]
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }
}

impl Drop for AskpassServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        // Wake the accept loop so its thread exits.
        let _ = UnixStream::connect(&self.socket_path);
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

fn serve(
    mut stream: UnixStream,
    token: &str,
    tx: &async_channel::Sender<AskpassRequest>,
) -> io::Result<()> {
    let mut request = Vec::new();
    (&mut stream).take(MAX_REQUEST).read_to_end(&mut request)?;

    let prompt = request.strip_prefix(MAGIC).and_then(|rest| {
        let newline = rest.iter().position(|b| *b == b'\n')?;
        let (sent_token, prompt) = (&rest[..newline], &rest[newline + 1..]);
        (sent_token == token.as_bytes()).then(|| String::from_utf8_lossy(prompt).into_owned())
    });
    let Some(prompt) = prompt else {
        return stream.write_all(b"n");
    };

    let (reply_tx, reply_rx) = mpsc::channel();
    let answer = match tx.send_blocking(AskpassRequest {
        prompt,
        reply: reply_tx,
    }) {
        Ok(()) => reply_rx.recv().ok().flatten(),
        Err(_) => None, // the app is shutting down
    };
    match answer {
        Some(answer) => {
            stream.write_all(b"y")?;
            stream.write_all(answer.as_bytes())
        }
        None => stream.write_all(b"n"),
    }
}

/// Client side: sends `prompt` and waits for the user's answer (`None` = cancelled).
pub fn request(socket: &Path, token: &str, prompt: &str) -> io::Result<Option<String>> {
    let mut stream = UnixStream::connect(socket)?;
    stream.write_all(MAGIC)?;
    stream.write_all(token.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.write_all(prompt.as_bytes())?;
    stream.shutdown(Shutdown::Write)?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response)?;
    Ok(match response.split_first() {
        Some((b'y', answer)) => Some(String::from_utf8_lossy(answer).into_owned()),
        _ => None,
    })
}

/// Entry point of helper mode: `gitweft "<prompt>"` with [`ENV_SOCKET`] set.
pub fn run_helper(prompt: &str) -> ExitCode {
    if std::env::var_os(ENV_REFUSE).is_some_and(|value| value == "1") {
        // git reports "could not read Password", ssh falls back to its next method or
        // "Host key verification failed"; either way the run fails without asking.
        eprintln!("gitweft askpass: prompts are disabled for this operation");
        return ExitCode::FAILURE;
    }
    let (Some(socket), Some(token)) = (std::env::var_os(ENV_SOCKET), std::env::var(ENV_TOKEN).ok())
    else {
        eprintln!("gitweft askpass: not started by Gitweft");
        return ExitCode::FAILURE;
    };
    match request(Path::new(&socket), &token, prompt) {
        Ok(Some(answer)) => {
            println!("{answer}");
            ExitCode::SUCCESS
        }
        Ok(None) => ExitCode::FAILURE,
        Err(err) => {
            eprintln!("gitweft askpass: cannot reach Gitweft: {err}");
            ExitCode::FAILURE
        }
    }
}

fn random_token() -> io::Result<String> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn short_dir() -> tempfile::TempDir {
        // Unix socket paths are limited to ~108 bytes; keep them short.
        tempfile::Builder::new()
            .prefix("gw")
            .tempdir_in("/tmp")
            .unwrap()
    }

    #[test]
    fn prompt_round_trip() {
        let dir = short_dir();
        let (server, requests) = AskpassServer::start_in(dir.path()).unwrap();
        let answerer = thread::spawn(move || {
            let req = requests.recv_blocking().unwrap();
            assert_eq!(req.kind(), PromptKind::Secret);
            let prompt = req.prompt.clone();
            req.answer("hunter2".into());
            prompt
        });
        let answer = request(
            server.socket_path(),
            &server.token,
            "Password for 'https://h': ",
        )
        .unwrap();
        assert_eq!(answer.as_deref(), Some("hunter2"));
        assert_eq!(answerer.join().unwrap(), "Password for 'https://h': ");
    }

    #[test]
    fn cancel_and_dropped_requests_answer_none() {
        let dir = short_dir();
        let (server, requests) = AskpassServer::start_in(dir.path()).unwrap();
        let answerer = thread::spawn(move || {
            requests.recv_blocking().unwrap().cancel();
            drop(requests.recv_blocking().unwrap());
        });
        assert_eq!(
            request(server.socket_path(), &server.token, "a").unwrap(),
            None
        );
        assert_eq!(
            request(server.socket_path(), &server.token, "b").unwrap(),
            None
        );
        answerer.join().unwrap();
    }

    #[test]
    fn wrong_token_is_refused_without_bothering_the_user() {
        let dir = short_dir();
        let (server, requests) = AskpassServer::start_in(dir.path()).unwrap();
        assert_eq!(
            request(server.socket_path(), "wrong", "Password: ").unwrap(),
            None
        );
        assert!(requests.try_recv().is_err(), "no prompt may reach the UI");
    }

    #[test]
    fn socket_is_private() {
        let dir = short_dir();
        let (server, _requests) = AskpassServer::start_in(&dir.path().join("rt")).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir.path().join("rt")), 0o700);
        assert_eq!(mode(server.socket_path()), 0o600);
        let path = server.socket_path().to_owned();
        drop(server);
        assert!(!path.exists(), "socket file is removed on drop");
    }

    #[test]
    fn prompt_kinds() {
        let kind = |prompt: &str| {
            let (reply, _) = mpsc::channel();
            AskpassRequest {
                prompt: prompt.into(),
                reply,
            }
            .kind()
        };
        assert_eq!(
            kind("Username for 'https://example.com': "),
            PromptKind::Text
        );
        assert_eq!(
            kind("Password for 'https://u@example.com': "),
            PromptKind::Secret
        );
        assert_eq!(
            kind("Enter passphrase for key '/home/u/.ssh/id_ed25519': "),
            PromptKind::Secret
        );
        assert_eq!(
            kind("Are you sure you want to continue connecting (yes/no/[fingerprint])? "),
            PromptKind::Confirm
        );
    }
}
