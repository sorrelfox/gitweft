//! The twin runs the `gitweft` next to it: here a shell script standing in for it, which
//! prints its arguments, writes to stderr and exits with 3.

#![cfg(unix)]

use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::process::Command;

const TWIN: &str = env!("CARGO_BIN_EXE_gitweft-console");

#[test]
fn passes_on_the_arguments_the_output_and_the_exit_code() {
    let dir = tempfile::tempdir().unwrap();
    let twin = dir.path().join("gitweft-console");
    fs::copy(TWIN, &twin).unwrap();

    #[allow(clippy::disallowed_methods)] // The program under test, not git.
    let out = Command::new(&twin).arg("--version").output().unwrap();
    assert_eq!(out.status.code(), Some(1), "without a gitweft next to it");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.starts_with("gitweft: can't run "), "{stderr}");

    let app = dir.path().join("gitweft");
    fs::write(
        &app,
        "#!/bin/sh\nprintf '[%s]\\n' \"$@\"\necho to stderr >&2\nexit 3\n",
    )
    .unwrap();
    gitweft_platform::fs::set_executable(&app, true).unwrap();
    #[allow(clippy::disallowed_methods)] // The program under test, not git.
    let out = Command::new(&twin)
        .args([OsStr::new("a b"), OsStr::new(""), OsStr::new("\"q\"")])
        .arg(OsStr::from_bytes(b"caf\xe9"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3));
    assert_eq!(out.stdout, b"[a b]\n[]\n[\"q\"]\n[caf\xe9]\n");
    assert_eq!(out.stderr, b"to stderr\n");
}
