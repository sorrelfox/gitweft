//! AF_UNIX stream sockets through Winsock (Windows 10 1803 and later). The socket is a
//! file, as on Unix, so the folder it lives in decides who may connect, and the protocol
//! and timeouts above read the same on every system.
//!
//! Windows reads `sun_path` as a NUL-terminated UTF-8 path. A path that isn't valid
//! Unicode, holds a NUL or takes more than 107 bytes in UTF-8 can't name a socket and is
//! refused with `InvalidInput`.

use std::io::{self, Read, Write};
use std::iter::FusedIterator;
use std::mem::{self, MaybeUninit};
use std::net::Shutdown;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use windows_sys::Win32::Networking::WinSock::{
    AF_UNIX, INVALID_SOCKET, SD_BOTH, SD_RECEIVE, SD_SEND, SO_RCVTIMEO, SO_SNDTIMEO, SOCK_STREAM,
    SOCKADDR, SOCKADDR_UN, SOCKET, SOCKET_ERROR, SOL_SOCKET, WSA_FLAG_NO_HANDLE_INHERIT,
    WSA_FLAG_OVERLAPPED, WSADATA, WSAESHUTDOWN, WSAGetLastError, WSASocketW, WSAStartup, accept,
    bind, closesocket, connect, listen, recv, send, setsockopt, shutdown,
};

/// How many connections may wait to be accepted, as std's own listeners allow.
const BACKLOG: i32 = 128;

/// A socket listening on a path.
#[derive(Debug)]
pub struct Listener(Socket);

impl Listener {
    /// Listens on `path`. A file already there, even one left behind by a listener that
    /// has gone, makes this fail with `AddrInUse`, as on Unix.
    pub fn bind(path: impl AsRef<Path>) -> io::Result<Self> {
        let (addr, len) = address(path.as_ref())?;
        let socket = Socket::new()?;
        // SAFETY: `addr` is a whole SOCKADDR_UN that lives across the call, and `len` covers
        // its family and its path up to and including the NUL, all inside it.
        check(unsafe { bind(socket.0, (&raw const addr).cast::<SOCKADDR>(), len) })?;
        // SAFETY: the socket is ours and bound.
        check(unsafe { listen(socket.0, BACKLOG) })?;
        Ok(Self(socket))
    }

    /// Waits for the next connection.
    pub fn accept(&self) -> io::Result<(Stream, SocketAddr)> {
        let mut addr = empty_address();
        let mut len = mem::size_of::<SOCKADDR_UN>() as i32;
        // SAFETY: `addr` and `len` live across the call, and `len` is the size of `addr`,
        // which is as much as the call writes.
        let socket = unsafe { accept(self.0.0, (&raw mut addr).cast::<SOCKADDR>(), &mut len) };
        if socket == INVALID_SOCKET {
            return Err(last_error());
        }
        // An accepted socket takes the listener's flags, so git children don't inherit it
        // either.
        Ok((Stream(Socket(socket)), SocketAddr::from_raw(&addr, len)))
    }

    /// The connections as they come, without end.
    pub fn incoming(&self) -> Incoming<'_> {
        Incoming { listener: self }
    }
}

/// The connections a [`Listener`] accepts, from [`Listener::incoming`].
#[derive(Debug)]
pub struct Incoming<'a> {
    listener: &'a Listener,
}

impl Iterator for Incoming<'_> {
    type Item = io::Result<Stream>;

    fn next(&mut self) -> Option<Self::Item> {
        Some(self.listener.accept().map(|(stream, _)| stream))
    }
}

impl FusedIterator for Incoming<'_> {}

/// The address of the other end of an accepted connection. A client that didn't bind its
/// socket to a path, which is every client here, has none.
#[derive(Clone, Debug)]
pub struct SocketAddr {
    path: Option<PathBuf>,
}

impl SocketAddr {
    fn from_raw(addr: &SOCKADDR_UN, len: i32) -> Self {
        let path_len = usize::try_from(len)
            .unwrap_or(0)
            .saturating_sub(mem::offset_of!(SOCKADDR_UN, sun_path))
            .min(addr.sun_path.len());
        let bytes: Vec<u8> = addr.sun_path[..path_len]
            .iter()
            .map(|&byte| byte as u8)
            .take_while(|&byte| byte != 0)
            .collect();
        let path = String::from_utf8(bytes)
            .ok()
            .filter(|path| !path.is_empty())
            .map(PathBuf::from);
        Self { path }
    }

    pub fn as_pathname(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn is_unnamed(&self) -> bool {
        self.path.is_none()
    }
}

/// One end of a connection.
#[derive(Debug)]
pub struct Stream(Socket);

impl Stream {
    pub fn connect(path: impl AsRef<Path>) -> io::Result<Self> {
        let (addr, len) = address(path.as_ref())?;
        let socket = Socket::new()?;
        // SAFETY: as in `Listener::bind`.
        check(unsafe { connect(socket.0, (&raw const addr).cast::<SOCKADDR>(), len) })?;
        Ok(Self(socket))
    }

    /// How long a read waits before failing with `TimedOut`; `None` waits for ever. A zero
    /// duration is refused with `InvalidInput`, as std refuses it.
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.0.set_timeout(SO_RCVTIMEO, timeout)
    }

    /// How long a write waits before failing with `TimedOut`; `None` waits for ever.
    pub fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.0.set_timeout(SO_SNDTIMEO, timeout)
    }

    /// Ends one or both directions; after `Shutdown::Write` the other end reads EOF.
    pub fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        let how = match how {
            Shutdown::Read => SD_RECEIVE,
            Shutdown::Write => SD_SEND,
            Shutdown::Both => SD_BOTH,
        };
        // SAFETY: the socket is ours and open.
        check(unsafe { shutdown(self.0.0, how) }).map(drop)
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        (&*self).read(buf)
    }
}

impl Read for &Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let len = buf.len().min(i32::MAX as usize) as i32;
        // SAFETY: the call writes at most `len` bytes, which is no more than `buf` holds.
        let read = unsafe { recv(self.0.0, buf.as_mut_ptr(), len, 0) };
        if read == SOCKET_ERROR {
            let err = last_error();
            // A socket whose reading side was shut down reads EOF on Unix; the same here.
            if err.raw_os_error() == Some(WSAESHUTDOWN) {
                return Ok(0);
            }
            return Err(err);
        }
        Ok(read as usize)
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        (&*self).write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Write for &Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let len = buf.len().min(i32::MAX as usize) as i32;
        // SAFETY: the call reads at most `len` bytes, which is no more than `buf` holds.
        let sent = unsafe { send(self.0.0, buf.as_ptr(), len, 0) };
        if sent == SOCKET_ERROR {
            return Err(last_error());
        }
        Ok(sent as usize)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// An open socket, closed on drop.
#[derive(Debug)]
struct Socket(SOCKET);

impl Socket {
    fn new() -> io::Result<Self> {
        startup()?;
        // Overlapped only because Winsock ignores the timeouts of a socket made without it;
        // every call here still blocks. Not inherited, so git's children can't hold it open.
        // SAFETY: no protocol info is passed, and the flags are ones WSASocketW takes.
        let socket = unsafe {
            WSASocketW(
                i32::from(AF_UNIX),
                SOCK_STREAM,
                0,
                std::ptr::null(),
                0,
                WSA_FLAG_OVERLAPPED | WSA_FLAG_NO_HANDLE_INHERIT,
            )
        };
        if socket == INVALID_SOCKET {
            return Err(last_error());
        }
        Ok(Self(socket))
    }

    fn set_timeout(&self, option: i32, timeout: Option<Duration>) -> io::Result<()> {
        let millis: u32 = match timeout {
            Some(timeout) if timeout.is_zero() => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "cannot set a 0 duration timeout",
                ));
            }
            // Rounded up, so a timeout under a millisecond doesn't become none at all.
            Some(timeout) => timeout
                .as_nanos()
                .div_ceil(1_000_000)
                .try_into()
                .unwrap_or(u32::MAX),
            None => 0,
        };
        // SAFETY: the option takes a DWORD, and `millis` is one that lives across the call.
        check(unsafe {
            setsockopt(
                self.0,
                SOL_SOCKET,
                option,
                (&raw const millis).cast::<u8>(),
                mem::size_of::<u32>() as i32,
            )
        })
        .map(drop)
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        // SAFETY: the socket is ours, and nothing uses it after this.
        unsafe {
            closesocket(self.0);
        }
    }
}

/// Starts Winsock for this process before its first socket. It is never cleaned up again,
/// which is what std's own sockets do too.
fn startup() -> io::Result<()> {
    static STARTED: OnceLock<i32> = OnceLock::new();
    let code = *STARTED.get_or_init(|| {
        let mut data = MaybeUninit::<WSADATA>::uninit();
        // SAFETY: the call fills `data`, which lives across it and is never read here.
        unsafe { WSAStartup(0x0202, data.as_mut_ptr()) }
    });
    match code {
        0 => Ok(()),
        code => Err(io::Error::from_raw_os_error(code)),
    }
}

fn empty_address() -> SOCKADDR_UN {
    SOCKADDR_UN {
        sun_family: AF_UNIX,
        sun_path: [0; 108],
    }
}

/// `path` as a socket address, and the length to pass with it: the family, the path and
/// its NUL.
fn address(path: &Path) -> io::Result<(SOCKADDR_UN, i32)> {
    let invalid = |why: &str| {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} {why}, so it can't name a socket", path.display()),
        ))
    };
    let Some(text) = path.to_str() else {
        return invalid("isn't valid Unicode");
    };
    let bytes = text.as_bytes();
    let mut addr = empty_address();
    if bytes.is_empty() {
        return invalid("is empty");
    }
    if bytes.contains(&0) {
        return invalid("holds a NUL");
    }
    if bytes.len() >= addr.sun_path.len() {
        return invalid(&format!(
            "takes more than {} bytes",
            addr.sun_path.len() - 1
        ));
    }
    for (slot, &byte) in addr.sun_path.iter_mut().zip(bytes) {
        *slot = byte as i8;
    }
    let len = mem::offset_of!(SOCKADDR_UN, sun_path) + bytes.len() + 1;
    Ok((addr, len as i32))
}

fn check(ret: i32) -> io::Result<i32> {
    if ret == SOCKET_ERROR {
        Err(last_error())
    } else {
        Ok(ret)
    }
}

fn last_error() -> io::Error {
    // SAFETY: only reads this thread's last Winsock error.
    io::Error::from_raw_os_error(unsafe { WSAGetLastError() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::ffi::OsStringExt;
    use std::thread;

    fn short_dir() -> tempfile::TempDir {
        tempfile::Builder::new().prefix("gw").tempdir().unwrap()
    }

    #[test]
    fn a_request_and_its_reply_cross() {
        let dir = short_dir();
        let path = dir.path().join("rt.sock");
        let listener = Listener::bind(&path).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, addr) = listener.accept().unwrap();
            assert!(addr.is_unnamed());
            let mut request = Vec::new();
            stream.read_to_end(&mut request).unwrap();
            stream.write_all(b"ok\n").unwrap();
            request
        });
        let mut stream = Stream::connect(&path).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream.write_all(b"hello\0").unwrap();
        stream.shutdown(Shutdown::Write).unwrap();
        let mut reply = Vec::new();
        stream.read_to_end(&mut reply).unwrap();
        assert_eq!(reply, b"ok\n");
        assert_eq!(server.join().unwrap(), b"hello\0");
    }

    #[test]
    fn incoming_yields_each_connection() {
        let dir = short_dir();
        let path = dir.path().join("in.sock");
        let listener = Listener::bind(&path).unwrap();
        let server = thread::spawn(move || {
            for stream in listener.incoming().take(2) {
                (&stream.unwrap()).write_all(b"y").unwrap();
            }
        });
        for _ in 0..2 {
            let mut reply = Vec::new();
            Stream::connect(&path)
                .unwrap()
                .read_to_end(&mut reply)
                .unwrap();
            assert_eq!(reply, b"y");
        }
        server.join().unwrap();
    }

    #[test]
    fn a_read_with_nothing_coming_times_out() {
        let dir = short_dir();
        let path = dir.path().join("to.sock");
        let listener = Listener::bind(&path).unwrap();
        let stream = Stream::connect(&path).unwrap();
        let (_held, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let err = (&stream).read(&mut [0; 8]).unwrap_err();
        assert!(
            matches!(
                err.kind(),
                io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
            ),
            "{err}"
        );
    }

    #[test]
    fn a_zero_timeout_is_refused() {
        let dir = short_dir();
        let path = dir.path().join("zero.sock");
        let _listener = Listener::bind(&path).unwrap();
        let stream = Stream::connect(&path).unwrap();
        let err = stream.set_read_timeout(Some(Duration::ZERO)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        stream.set_read_timeout(None).unwrap();
    }

    #[test]
    fn a_socket_file_already_there_is_in_use_until_removed() {
        let dir = short_dir();
        let path = dir.path().join("in-use.sock");
        let listener = Listener::bind(&path).unwrap();
        let err = Listener::bind(&path).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse);

        // Gone, but its file stays: nobody accepts, and the name is still taken.
        drop(listener);
        assert!(std::fs::symlink_metadata(&path).is_ok());
        assert!(Stream::connect(&path).is_err());
        let err = Listener::bind(&path).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse);

        std::fs::remove_file(&path).unwrap();
        let _listener = Listener::bind(&path).unwrap();
        Stream::connect(&path).unwrap();
    }

    #[test]
    fn nothing_listening_is_an_error() {
        let dir = short_dir();
        assert!(Stream::connect(dir.path().join("none.sock")).is_err());
    }

    #[test]
    fn a_path_with_letters_beyond_ascii_works() {
        let dir = short_dir();
        let path = dir.path().join("caf\u{e9}-\u{0436}.sock");
        let _listener = Listener::bind(&path).unwrap();
        Stream::connect(&path).unwrap();
    }

    #[test]
    fn paths_that_cant_name_a_socket_are_refused() {
        let refused = |path: &Path| address(path).err().map(|err| err.kind());
        let invalid = Some(io::ErrorKind::InvalidInput);
        assert_eq!(
            refused(Path::new(&format!("C:\\{}", "a".repeat(105)))),
            invalid
        );
        let (_, len) = address(Path::new(&format!("C:\\{}", "a".repeat(104)))).unwrap();
        assert_eq!(len, 2 + 107 + 1);
        assert_eq!(refused(Path::new("C:\\a\0b")), invalid);
        assert_eq!(refused(Path::new("")), invalid);
        // A lone surrogate: a valid Windows file name, but not Unicode.
        let unpaired = std::ffi::OsString::from_wide(&[0x43, 0x3a, 0xd800]);
        assert_eq!(refused(Path::new(&unpaired)), invalid);
    }
}
