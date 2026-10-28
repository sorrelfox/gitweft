//! `gitweft.com`, the console twin of the Windows build. A release `gitweft.exe` is a GUI
//! program, and cmd and PowerShell don't wait for those: `gitweft --version` would print
//! after the prompt is back, and the exit code would be lost. For a bare `gitweft`, both
//! shells try `.com` before `.exe` (PATHEXT), so they start this console program instead,
//! which runs the `gitweft.exe` next to it with the same arguments, waits for it, and exits
//! with its exit code. Explorer, the shortcuts and git run gitweft.exe by its full path.
//!
//! On other systems it runs the `gitweft` next to it, which only the tests need.

use std::env;
use std::process::{self, Command};

fn main() {
    let app = match env::current_exe() {
        Ok(exe) => exe.with_file_name(format!("gitweft{}", env::consts::EXE_SUFFIX)),
        Err(err) => {
            eprintln!("gitweft: can't find the folder of this program: {err}");
            process::exit(1);
        }
    };
    // Standard input and output are inherited, so gitweft.exe prints to this console (or to
    // the file or pipe it was sent to). Ctrl+C ends this program as usual; a window it
    // started stays open, because the window isn't attached to the console.
    #[allow(clippy::disallowed_methods)] // Gitweft itself, not git.
    let status = Command::new(&app).args(env::args_os().skip(1)).status();
    match status {
        // All 32 bits of a Windows exit code go through `exit`. There is no code only on
        // Unix, for a process ended by a signal.
        Ok(status) => process::exit(status.code().unwrap_or(1)),
        Err(err) => {
            eprintln!("gitweft: can't run {}: {err}", app.display());
            process::exit(1);
        }
    }
}
