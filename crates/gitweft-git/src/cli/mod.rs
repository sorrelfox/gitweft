//! Supervised execution of the system `git` binary.

mod error;
mod progress;
mod runner;
mod version;

pub use error::{GitError, GitErrorKind, classify};
pub use progress::{Progress, StderrEvent, StderrSplitter, parse_stderr_line};
pub use runner::{GitCli, GitCommand, GitOutput};
pub use version::GitVersion;
