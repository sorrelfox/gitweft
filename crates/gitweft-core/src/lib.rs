//! Gitweft's application core. Nothing in this crate depends on the UI toolkit, so it
//! can be tested without a window.

pub mod conflicts;
pub mod datafile;
pub mod external;
pub mod ipc;
pub mod jobs;
pub mod keymap;
pub mod oplog;
pub mod paths;
pub mod recent;
pub mod repo;
pub mod repo_id;
pub mod repo_settings;
pub mod review;
pub mod session;
pub mod settings;
pub mod trash;
pub mod watch;
