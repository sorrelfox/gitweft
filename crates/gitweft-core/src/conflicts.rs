//! The files an operation stopped on, kept for the conflicted-files panel so the ones
//! already resolved are still listed after a restart (git's own list, in `MERGE_MSG`,
//! isn't always written, and it's gone once the message is edited).
//!
//! Kept in `<repo data dir>/conflicts` as lines of text, because paths need not be
//! UTF-8. Each file is written as hex:
//!
//! ```text
//! gitweft-conflicts 1
//! step merge 0123abcd…
//! file 6e6f7465732e747874
//! ```
//!
//! The step names the operation and the commit HEAD was on; a record for another step is
//! stale and ignored. Reading is lenient: unknown or broken lines are skipped.

use std::io;
use std::path::{Path, PathBuf};

use crate::paths::repo_data_dir;

const HEADER: &str = "gitweft-conflicts";
const VERSION: u32 = 1;
/// More files than this aren't kept.
pub const MAX_FILES: usize = 10_000;

/// `conflicts` in the repository's data dir (`root`: its working tree).
pub fn conflicts_file(root: &Path) -> PathBuf {
    repo_data_dir(root).join("conflicts")
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConflictRecord {
    /// The operation and HEAD's commit, as the caller names them (one line, no newline).
    pub step: String,
    /// Every file conflicted at that step, resolved ones included, in the panel's order.
    pub files: Vec<Vec<u8>>,
}

/// The record in `file`; `None` when it's missing, unreadable or not such a file.
pub fn read(file: &Path) -> Option<ConflictRecord> {
    let text = std::fs::read_to_string(file).ok()?;
    parse(&text)
}

fn parse(text: &str) -> Option<ConflictRecord> {
    let mut lines = text.lines();
    let header = lines.next()?;
    let version: u32 = header.strip_prefix(HEADER)?.trim().parse().ok()?;
    if version == 0 {
        return None;
    }
    let mut record = ConflictRecord::default();
    for line in lines {
        let (key, value) = line.split_once(' ').unwrap_or((line, ""));
        match key {
            "step" => record.step = value.to_owned(),
            "file" if record.files.len() < MAX_FILES => {
                if let Some(path) = from_hex(value)
                    && !path.is_empty()
                    && !record.files.contains(&path)
                {
                    record.files.push(path);
                }
            }
            _ => {}
        }
    }
    (!record.step.is_empty()).then_some(record)
}

fn to_text(record: &ConflictRecord) -> String {
    let mut text = format!(
        "{HEADER} {VERSION}\nstep {}\n",
        record.step.replace('\n', " ")
    );
    for path in record.files.iter().take(MAX_FILES) {
        text.push_str("file ");
        for byte in path {
            text.push_str(&format!("{byte:02x}"));
        }
        text.push('\n');
    }
    text
}

fn from_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(text.get(at..at + 2)?, 16).ok())
        .collect()
}

/// Writes `record` to `file` (a temporary file, then a rename), creating its folder.
pub fn write(file: &Path, record: &ConflictRecord) -> io::Result<()> {
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    crate::datafile::atomic_write(file, to_text(record).as_bytes())
}

/// Removes the record (the operation is over); a missing file is fine.
pub fn remove(file: &Path) -> io::Result<()> {
    match std::fs::remove_file(file) {
        Err(err) if err.kind() != io::ErrorKind::NotFound => Err(err),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_round_trips_with_raw_paths() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("repos/r/conflicts");
        let record = ConflictRecord {
            step: "merge 0123abcd".into(),
            files: vec![b"notes.txt".to_vec(), b"src/\xff odd\nname".to_vec()],
        };
        write(&file, &record).unwrap();
        assert!(!file.with_extension("tmp").exists());
        assert_eq!(read(&file), Some(record));
        remove(&file).unwrap();
        assert_eq!(read(&file), None);
        remove(&file).unwrap();
    }

    #[test]
    fn reading_is_lenient() {
        let text = "gitweft-conflicts 1\nstep rebase -\nfile 61\nwhat is this\nfile zz\n\
                    file 623\nfile 61\nfile \nfuture key\nfile 62\n";
        assert_eq!(
            parse(text),
            Some(ConflictRecord {
                step: "rebase -".into(),
                files: vec![b"a".to_vec(), b"b".to_vec()],
            })
        );
        assert_eq!(parse("gitweft-conflicts 1\nfile 61\n"), None, "no step");
        assert_eq!(parse("gitweft-session 1\nstep merge x\n"), None);
        assert_eq!(parse(""), None);
        // A newer version's file is still read.
        assert!(parse("gitweft-conflicts 7\nstep merge x\n").is_some());
    }

    #[test]
    fn the_file_is_in_the_repository_data_dir() {
        let root = Path::new("/work/project");
        assert_eq!(conflicts_file(root).parent().unwrap(), repo_data_dir(root));
    }
}
