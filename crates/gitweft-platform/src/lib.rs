//! The seams between Gitweft and the system it runs on: paths that arrive as bytes, the
//! folders Gitweft keeps its files in, file permissions, child processes and the local
//! sockets it talks to itself over. This is the only crate that calls libc or the Win32
//! API, and the only one that cares which system this is; everything above it reads the
//! same on Linux, macOS and Windows.

use std::io;

pub mod bytes;
pub mod dirs;
pub mod fs;
pub mod local_socket;
pub mod process;

/// Fills `buf` with random bytes from the system's own generator.
pub fn random_bytes(buf: &mut [u8]) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Read;

        std::fs::File::open("/dev/urandom")?.read_exact(buf)
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Security::Cryptography::{
            BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom,
        };

        // One call per u32's worth, since that is the length the call takes.
        for chunk in buf.chunks_mut(u32::MAX as usize) {
            let len = chunk.len() as u32;
            // SAFETY: the call writes `len` bytes, which is this chunk's own length, and
            // the null algorithm handle together with the flag asks for the system
            // generator rather than one we would have to open first.
            let status = unsafe {
                BCryptGenRandom(
                    std::ptr::null_mut(),
                    chunk.as_mut_ptr(),
                    len,
                    BCRYPT_USE_SYSTEM_PREFERRED_RNG,
                )
            };
            if status < 0 {
                return Err(io::Error::other(format!(
                    "the system random generator failed ({status:#010x})"
                )));
            }
        }
        Ok(())
    }
}

/// glibc gives threads that allocate at the same time arenas of their own, up to eight per
/// core, and freed memory stays in them. On the Linux kernel's history the job threads'
/// arenas kept about 25 MB resident after the load; two arenas keep it to a few, with no
/// measurable cost to load time. Called first, before any thread starts. A limit the
/// user set (`MALLOC_ARENA_MAX`, or `arena_max` in `GLIBC_TUNABLES`) is left alone.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub fn limit_malloc_arenas() {
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
pub fn limit_malloc_arenas() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_bytes_differ() {
        let mut first = [0u8; 32];
        let mut second = [0u8; 32];
        random_bytes(&mut first).unwrap();
        random_bytes(&mut second).unwrap();
        assert_ne!(first, second);
        assert_ne!(first, [0u8; 32]);
    }

    #[test]
    fn no_bytes_wanted_is_no_error() {
        random_bytes(&mut []).unwrap();
    }
}
