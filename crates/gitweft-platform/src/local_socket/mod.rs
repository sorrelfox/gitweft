//! The local stream sockets Gitweft talks to itself over: the single-instance hand-off,
//! which gives a repository's path to the app already running, and the askpass relay, which
//! carries a prompt from a git child to the app and the answer back. Both ends belong to
//! the same user on the same machine, and nothing here reaches the network.
//!
//! A socket is a path in a folder only this user may enter ([`crate::dirs::socket_dir`]).
//! Windows has had the same AF_UNIX sockets since Windows 10 but no standard library
//! wrapper for them, so [`Listener`] and [`Stream`] there are ours, with the methods the
//! Unix ones have.

#[cfg(unix)]
pub use std::os::unix::net::{
    Incoming, SocketAddr, UnixListener as Listener, UnixStream as Stream,
};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{Incoming, Listener, SocketAddr, Stream};
