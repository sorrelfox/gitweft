//! One Gitweft per user and data dir: `gitweft PATH` hands PATH to the Gitweft already
//! running, which opens it in a tab (or switches to the tab that shows it) and comes
//! forward; the new process then exits.
//!
//! The running app listens on a Unix socket in `$XDG_RUNTIME_DIR/gitweft/` (a 0700 folder,
//! the socket 0600, so only the same user can connect). Its name carries a hash of the
//! data dir: an instance started with another `XDG_DATA_HOME` (a test, a second profile)
//! has its own recent list and session, and so its own socket.
//!
//! Wire protocol (one request per connection):
//! client → server: `MAGIC`, then each absolute path's bytes followed by a NUL, then
//! shutdown(write). No paths means "come forward".
//! server → client: `ok\n` once the request is queued for the app.

use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use crate::paths::{data_dir, fnv1a};

const MAGIC: &[u8] = b"gitweft-ipc-1\n";
const REPLY: &[u8] = b"ok\n";
const MAX_REQUEST: u64 = 1024 * 1024;
/// How long a client waits for a running app that accepted the connection to answer.
const TIMEOUT: Duration = Duration::from_secs(5);

/// The socket of the Gitweft using this process's data dir.
pub fn socket_path() -> PathBuf {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute());
    socket_path_in(runtime, &data_dir())
}

/// `<runtime dir>/gitweft/ipc-<hash of data_dir>.sock`. Without a runtime dir, a folder of
/// this user's own in the temp dir.
fn socket_path_in(runtime: Option<PathBuf>, data_dir: &Path) -> PathBuf {
    let dir = runtime.map_or_else(
        || std::env::temp_dir().join(format!("gitweft-{}", crate::paths::own_uid())),
        |runtime| runtime.join("gitweft"),
    );
    let hash = fnv1a(data_dir.as_os_str().as_bytes());
    dir.join(format!("ipc-{hash:016x}.sock"))
}

/// Hands `paths` (absolute) to the Gitweft listening on `socket`. An error means the
/// caller starts the app itself: none is running there, or one took the connection but
/// didn't answer in time, which [`is_no_answer`] tells apart (#22).
pub fn send(socket: &Path, paths: &[PathBuf]) -> io::Result<()> {
    send_within(socket, paths, TIMEOUT)
}

fn send_within(socket: &Path, paths: &[PathBuf], timeout: Duration) -> io::Result<()> {
    // Only to a socket in a folder of our own: another user may have made the fallback
    // folder in /tmp to receive our repositories' paths (#20).
    if let Some(dir) = socket.parent() {
        check_private(dir, crate::paths::own_uid())?;
    }
    let request = encode(paths)?;
    let mut stream = UnixStream::connect(socket)?;
    // Connected: a Gitweft is there, so anything from here on is it not answering.
    let answered = (|| {
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        stream.write_all(&request)?;
        stream.shutdown(Shutdown::Write)?;
        let mut reply = Vec::new();
        (&mut stream)
            .take(REPLY.len() as u64)
            .read_to_end(&mut reply)?;
        if reply == REPLY {
            Ok(())
        } else {
            Err(io::Error::other(
                "it closed the connection without answering",
            ))
        }
    })();
    answered.map_err(|err| {
        // A read or write that ran out of time says so, rather than "resource unavailable".
        let err = match err.kind() {
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => io::Error::new(
                io::ErrorKind::TimedOut,
                format!("nothing within {} s", timeout.as_secs_f32()),
            ),
            _ => err,
        };
        io::Error::new(err.kind(), NoAnswer(err))
    })
}

/// The running Gitweft took the connection but didn't answer.
#[derive(Debug)]
struct NoAnswer(io::Error);

impl std::fmt::Display for NoAnswer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // What went wrong only: the messages around it say that it didn't answer.
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for NoAnswer {}

/// Whether [`send`] failed because a Gitweft is running but didn't answer (hung, or too
/// busy), rather than because none is running: then starting another one means two
/// windows the next `gitweft PATH` can't both reach, which is worth saying (#22).
pub fn is_no_answer(err: &io::Error) -> bool {
    err.get_ref().is_some_and(|inner| inner.is::<NoAnswer>())
}

fn encode(paths: &[PathBuf]) -> io::Result<Vec<u8>> {
    let mut bytes = MAGIC.to_vec();
    for path in paths {
        let raw = path.as_os_str().as_bytes();
        if !path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "paths sent to Gitweft must be absolute",
            ));
        }
        bytes.extend_from_slice(raw);
        bytes.push(0);
    }
    Ok(bytes)
}

/// The paths of a request, or `None` when it isn't one (a probe, a stranger).
fn decode(request: &[u8]) -> Option<Vec<PathBuf>> {
    let rest = request.strip_prefix(MAGIC)?;
    let paths = rest
        .split(|&byte| byte == 0)
        .filter(|raw| !raw.is_empty())
        .map(|raw| PathBuf::from(std::ffi::OsStr::from_bytes(raw)))
        .filter(|path| path.is_absolute())
        .collect();
    Some(paths)
}

/// Listens for other `gitweft` processes for as long as it lives; each request's paths
/// arrive on the channel returned by [`IpcServer::start`].
pub struct IpcServer {
    socket_path: PathBuf,
    shutdown: Arc<AtomicBool>,
}

impl IpcServer {
    /// Listens on `socket`, replacing a socket file a crashed Gitweft left behind. Fails
    /// with `AddrInUse` when a live Gitweft listens there already.
    pub fn start(socket: &Path) -> io::Result<(Self, async_channel::Receiver<Vec<PathBuf>>)> {
        Self::start_with(socket, |accept| {
            thread::Builder::new()
                .name("gitweft-ipc".into())
                .spawn(accept)
                .map(drop)
        })
    }

    /// [`IpcServer::start`], with `spawn` starting the accept loop on its own thread (a
    /// test makes it fail).
    fn start_with(
        socket: &Path,
        spawn: impl FnOnce(Box<dyn FnOnce() + Send>) -> io::Result<()>,
    ) -> io::Result<(Self, async_channel::Receiver<Vec<PathBuf>>)> {
        if let Some(dir) = socket.parent() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)?;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
            check_private(dir, crate::paths::own_uid())?;
        }
        let shutdown = Arc::new(AtomicBool::new(false));
        let (tx, rx) = async_channel::unbounded();
        // One start at a time from the stale check until it serves: two that both found a
        // stale socket would each remove and bind it, and both run (#20). And everything
        // that can still fail after the bind happens under the lock too: a start that bound,
        // let the lock go and then failed (its accept thread not spawned, under load) had
        // meanwhile refused the next start as in use, so neither ran (#65). A failure after
        // the bind removes the socket, which is this start's own, for the next start.
        crate::datafile::with_lock(socket, || {
            let listener = match UnixListener::bind(socket) {
                Ok(listener) => listener,
                Err(err) if err.kind() == io::ErrorKind::AddrInUse => {
                    // A live app accepts the connection; a stale file refuses it.
                    if UnixStream::connect(socket).is_ok() {
                        return Err(err);
                    }
                    std::fs::remove_file(socket)?;
                    UnixListener::bind(socket)?
                }
                Err(err) => return Err(err),
            };
            let serve_from = || -> io::Result<()> {
                std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))?;
                let shutdown = Arc::clone(&shutdown);
                spawn(Box::new(move || {
                    for stream in listener.incoming() {
                        if shutdown.load(Ordering::Relaxed) {
                            break;
                        }
                        let Ok(stream) = stream else { continue };
                        let tx = tx.clone();
                        // A slow client must not hold up the next one.
                        thread::spawn(move || {
                            let _ = serve(stream, &tx);
                        });
                    }
                }))
            };
            serve_from().inspect_err(|_| {
                let _ = std::fs::remove_file(socket);
            })
        })?;
        Ok((
            Self {
                socket_path: socket.to_owned(),
                shutdown,
            },
            rx,
        ))
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }
}

impl Drop for IpcServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        // Wake the accept loop so its thread exits.
        let _ = UnixStream::connect(&self.socket_path);
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

/// `dir` holds our socket only if it's a real folder (not a link), owned by `uid`, that
/// no one else may enter (#20).
fn check_private(dir: &Path, uid: u32) -> io::Result<()> {
    let meta = std::fs::symlink_metadata(dir)?;
    let refused = |why: &str| {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} {why}, so it can't hold Gitweft's socket", dir.display()),
        ))
    };
    if !meta.is_dir() {
        return refused("isn't a folder");
    }
    if meta.uid() != uid {
        return refused("belongs to another user");
    }
    if meta.mode() & 0o077 != 0 {
        return refused("is open to other users");
    }
    Ok(())
}

fn serve(mut stream: UnixStream, tx: &async_channel::Sender<Vec<PathBuf>>) -> io::Result<()> {
    stream.set_read_timeout(Some(TIMEOUT))?;
    let mut request = Vec::new();
    (&mut stream).take(MAX_REQUEST).read_to_end(&mut request)?;
    let Some(paths) = decode(&request) else {
        return Ok(());
    };
    if tx.send_blocking(paths).is_ok() {
        stream.write_all(REPLY)?;
    }
    Ok(())
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
    fn paths_reach_the_running_app() {
        let dir = short_dir();
        let socket = dir.path().join("rt/ipc.sock");
        let (_server, requests) = IpcServer::start(&socket).unwrap();
        let odd = PathBuf::from(std::ffi::OsStr::from_bytes(b"/repos/caf\xe9"));
        let paths = vec![PathBuf::from("/a b"), odd];
        send(&socket, &paths).unwrap();
        assert_eq!(requests.recv_blocking().unwrap(), paths);
        // No paths: "come forward".
        send(&socket, &[]).unwrap();
        assert_eq!(requests.recv_blocking().unwrap(), Vec::<PathBuf>::new());
    }

    #[test]
    fn relative_paths_are_refused_before_connecting_anywhere() {
        let err = encode(&[PathBuf::from("relative")]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn nothing_running_is_an_error() {
        let dir = short_dir();
        let err = send(&dir.path().join("none.sock"), &[]).unwrap_err();
        assert!(!is_no_answer(&err), "nothing is running: {err}");
    }

    /// A Gitweft that takes the connection and never answers (hung) is told apart from none
    /// running, so the new one can say why there are two (#22).
    #[test]
    fn a_running_app_that_doesnt_answer_is_told_apart() {
        let dir = short_dir();
        // As a server makes it: `send` only uses a folder of our own.
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = dir.path().join("ipc.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let hung = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            thread::sleep(Duration::from_millis(500));
            drop(stream);
        });
        let err = send_within(&socket, &[], Duration::from_millis(100)).unwrap_err();
        assert!(is_no_answer(&err), "{err}");
        hung.join().unwrap();

        // One that answers wrongly counts the same.
        let socket = dir.path().join("rude.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let rude = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.write_all(b"no\n").unwrap();
        });
        let err = send_within(&socket, &[], Duration::from_millis(500)).unwrap_err();
        assert!(is_no_answer(&err), "{err}");
        rude.join().unwrap();
    }

    #[test]
    fn a_live_server_keeps_its_socket_and_a_stale_one_is_replaced() {
        let dir = short_dir();
        let socket = dir.path().join("ipc.sock");
        let (server, requests) = IpcServer::start(&socket).unwrap();
        let err = IpcServer::start(&socket).err().expect("a second server");
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse);
        // The probe of the refused start isn't a request.
        send(&socket, &[PathBuf::from("/x")]).unwrap();
        assert_eq!(requests.recv_blocking().unwrap(), [PathBuf::from("/x")]);
        drop(server);
        assert!(!socket.exists(), "the socket file is removed on drop");

        // A crashed app's socket file: nobody accepts on it.
        drop(UnixListener::bind(&socket).unwrap());
        assert!(socket.exists());
        assert!(send(&socket, &[]).is_err());
        let (_server, requests) = IpcServer::start(&socket).unwrap();
        send(&socket, &[PathBuf::from("/y")]).unwrap();
        assert_eq!(requests.recv_blocking().unwrap(), [PathBuf::from("/y")]);
    }

    #[test]
    fn strangers_are_ignored() {
        let dir = short_dir();
        let socket = dir.path().join("ipc.sock");
        let (_server, requests) = IpcServer::start(&socket).unwrap();
        let mut stream = UnixStream::connect(&socket).unwrap();
        stream.write_all(b"hello\0/etc\0").unwrap();
        stream.shutdown(Shutdown::Write).unwrap();
        let mut reply = Vec::new();
        stream.read_to_end(&mut reply).unwrap();
        assert!(reply.is_empty());
        assert!(requests.try_recv().is_err());
    }

    #[test]
    fn socket_is_private() {
        let dir = short_dir();
        let socket = dir.path().join("rt/ipc.sock");
        let (_server, _requests) = IpcServer::start(&socket).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir.path().join("rt")), 0o700);
        assert_eq!(mode(&socket), 0o600);
    }

    #[test]
    fn only_a_folder_of_our_own_holds_the_socket() {
        let dir = short_dir();
        let uid = crate::paths::own_uid();
        let folder = dir.path().join("f");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&folder)
            .unwrap();
        check_private(&folder, uid).unwrap();
        // Another user's folder (#20): the owner is someone else.
        let err = check_private(&folder, uid.wrapping_add(1)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        // Open to others.
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(check_private(&folder, uid).is_err());
        // A link to a folder isn't the folder.
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o700)).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&folder, &link).unwrap();
        assert!(check_private(&link, uid).is_err());

        // So neither a client nor a server uses a socket there.
        let open = dir.path().join("open");
        std::fs::DirBuilder::new()
            .mode(0o755)
            .create(&open)
            .unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o755)).unwrap();
        let socket = open.join("ipc.sock");
        drop(UnixListener::bind(&socket).unwrap());
        assert_eq!(
            send(&socket, &[]).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn two_starts_on_a_stale_socket_make_one_server() {
        for _ in 0..10 {
            let dir = short_dir();
            let socket = dir.path().join("ipc.sock");
            // A crashed app's socket file.
            drop(UnixListener::bind(&socket).unwrap());
            let starts: Vec<_> = (0..2)
                .map(|_| {
                    let socket = socket.clone();
                    thread::spawn(move || IpcServer::start(&socket))
                })
                .collect();
            let results: Vec<_> = starts.into_iter().map(|t| t.join().unwrap()).collect();
            let started = results.iter().filter(|result| result.is_ok()).count();
            // What each start returned, so a failure says which step went wrong (#65).
            let outcomes: Vec<String> = results
                .iter()
                .map(|result| match result {
                    Ok(_) => "started".to_owned(),
                    Err(err) => format!("{:?}: {err}", err.kind()),
                })
                .collect();
            assert_eq!(
                started, 1,
                "one server, the other refused (#20): {outcomes:?}"
            );
            // The other was refused as in use, or, on a machine out of threads, failed to
            // start its own accept thread before the one that serves took the socket (#65).
            let refused = results.iter().find_map(|result| result.as_ref().err());
            assert!(
                matches!(
                    refused.unwrap().kind(),
                    io::ErrorKind::AddrInUse | io::ErrorKind::WouldBlock
                ),
                "the other start was refused as in use: {outcomes:?}"
            );
        }
    }

    /// A start that bound, then failed to spawn its accept thread (as under load), leaves
    /// the socket to a start waiting behind it, instead of refusing it as in use and
    /// failing too: exactly one serves (#65).
    #[test]
    fn a_start_that_fails_after_binding_leaves_the_socket_to_the_next() {
        let dir = short_dir();
        let socket = dir.path().join("ipc.sock");
        let failing = {
            let socket = socket.clone();
            thread::spawn(move || {
                IpcServer::start_with(&socket, |_| {
                    // Bound and listening while the other start comes.
                    thread::sleep(std::time::Duration::from_millis(300));
                    Err(io::Error::from(io::ErrorKind::WouldBlock))
                })
            })
        };
        thread::sleep(std::time::Duration::from_millis(50));
        let next = IpcServer::start(&socket);
        let failed = failing.join().unwrap();
        assert_eq!(
            failed.as_ref().err().map(io::Error::kind),
            Some(io::ErrorKind::WouldBlock)
        );
        let (server, requests) = next.expect("the next start serves");
        send(server.socket_path(), &[PathBuf::from("/a")]).unwrap();
        assert_eq!(requests.recv_blocking().unwrap(), [PathBuf::from("/a")]);
    }

    #[test]
    fn each_data_dir_has_its_own_socket() {
        let runtime = Some(PathBuf::from("/run/user/1000"));
        let a = socket_path_in(runtime.clone(), Path::new("/home/u/.local/share/gitweft"));
        let b = socket_path_in(runtime, Path::new("/tmp/test/data/gitweft"));
        assert_ne!(a, b);
        assert!(a.starts_with("/run/user/1000/gitweft"));
        assert!(a.to_string_lossy().ends_with(".sock"));
        let fallback = socket_path_in(None, Path::new("/d"));
        assert!(fallback.starts_with(std::env::temp_dir()));
        assert!(
            fallback
                .parent()
                .unwrap()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("gitweft-")
        );
    }
}
