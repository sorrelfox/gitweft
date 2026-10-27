//! Parsing of git's `--progress` output on stderr.

/// One progress update, e.g. `Receiving objects:  45% (450/1000)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Progress {
    pub phase: String,
    pub percent: Option<u8>,
    pub current: u64,
    pub total: Option<u64>,
    /// Reported by the remote side (`remote: ...`).
    pub remote: bool,
}

/// A single stderr record from git.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StderrEvent {
    Progress(Progress),
    Line(String),
}

/// Splits a raw stderr byte stream into records. Git terminates progress updates
/// with `\r` and ordinary messages with `\n`, so both are record separators.
#[derive(Debug, Default)]
pub struct StderrSplitter {
    buf: Vec<u8>,
}

impl StderrSplitter {
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<String> {
        let mut records = Vec::new();
        for &byte in bytes {
            if byte == b'\r' || byte == b'\n' {
                self.flush_into(&mut records);
            } else {
                self.buf.push(byte);
            }
        }
        records
    }

    pub fn finish(&mut self) -> Option<String> {
        let mut records = Vec::new();
        self.flush_into(&mut records);
        records.pop()
    }

    fn flush_into(&mut self, records: &mut Vec<String>) {
        if !self.buf.is_empty() {
            records.push(String::from_utf8_lossy(&self.buf).into_owned());
            self.buf.clear();
        }
    }
}

pub fn parse_stderr_line(line: &str) -> StderrEvent {
    parse_progress(line)
        .map(StderrEvent::Progress)
        .unwrap_or_else(|| StderrEvent::Line(line.to_owned()))
}

fn parse_progress(line: &str) -> Option<Progress> {
    let (remote, body) = match line.strip_prefix("remote: ") {
        Some(body) => (true, body),
        None => (false, line),
    };
    let (phase, rest) = body.split_once(": ")?;
    if phase.is_empty() || !phase.chars().all(|c| c.is_ascii_alphabetic() || c == ' ') {
        return None;
    }
    let rest = rest.trim_start();
    let digits_len = rest.chars().take_while(char::is_ascii_digit).count();
    if digits_len == 0 {
        return None;
    }
    let (number, after) = rest.split_at(digits_len);

    if let Some(after_pct) = after.strip_prefix('%') {
        // `45% (450/1000)...`
        let percent = number.parse::<u8>().ok()?;
        let inner = after_pct.trim_start().strip_prefix('(')?;
        let (counts, _) = inner.split_once(')')?;
        let (current, total) = counts.split_once('/')?;
        Some(Progress {
            phase: phase.to_owned(),
            percent: Some(percent),
            current: current.parse().ok()?,
            total: Some(total.parse().ok()?),
            remote,
        })
    } else if after.is_empty() || after.starts_with(',') {
        // `Enumerating objects: 5, done.` / `Counting objects: 1234`
        Some(Progress {
            phase: phase.to_owned(),
            percent: None,
            current: number.parse().ok()?,
            total: None,
            remote,
        })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn progress(line: &str) -> Progress {
        match parse_stderr_line(line) {
            StderrEvent::Progress(p) => p,
            StderrEvent::Line(l) => panic!("expected progress, got line {l:?}"),
        }
    }

    #[test]
    fn percent_with_counts() {
        let p = progress("Receiving objects:  45% (450/1000), 1.20 MiB | 2.00 MiB/s");
        assert_eq!(p.phase, "Receiving objects");
        assert_eq!(p.percent, Some(45));
        assert_eq!((p.current, p.total), (450, Some(1000)));
        assert!(!p.remote);
    }

    #[test]
    fn remote_prefix() {
        let p = progress("remote: Compressing objects: 100% (3/3), done.");
        assert!(p.remote);
        assert_eq!(p.phase, "Compressing objects");
        assert_eq!(p.percent, Some(100));
    }

    #[test]
    fn count_only() {
        let p = progress("remote: Enumerating objects: 5, done.");
        assert_eq!((p.percent, p.current, p.total), (None, 5, None));
    }

    #[test]
    fn git_lfs_meter() {
        // git-lfs pads its records with spaces (and ends them with `\r`, on stdout).
        let p = progress("Downloading LFS objects:  50% (1/2), 10 B | 1 KiB/s                  ");
        assert_eq!(p.phase, "Downloading LFS objects");
        assert_eq!((p.percent, p.current, p.total), (Some(50), 1, Some(2)));
        let p = progress("Uploading LFS objects: 100% (3/3), 3.0 MB | 0 B/s, done.");
        assert_eq!(
            (p.phase.as_str(), p.percent),
            ("Uploading LFS objects", Some(100))
        );
    }

    #[test]
    fn plain_messages_are_lines() {
        for line in [
            "fatal: not a git repository (or any of the parent directories): .git",
            "To /tmp/remote.git",
            "error: failed to push some refs to 'origin'",
            "hint: Updates were rejected because the tip of your current branch is behind",
        ] {
            assert_eq!(parse_stderr_line(line), StderrEvent::Line(line.to_owned()));
        }
    }

    #[test]
    fn splitter_handles_cr_and_partial_chunks() {
        let mut s = StderrSplitter::default();
        assert_eq!(
            s.feed(b"Receiving objects:  1% (1/100)\rReceiv"),
            vec!["Receiving objects:  1% (1/100)"]
        );
        assert_eq!(
            s.feed(b"ing objects: 100% (100/100), done.\nfatal: x"),
            vec!["Receiving objects: 100% (100/100), done."]
        );
        assert_eq!(s.finish().as_deref(), Some("fatal: x"));
        assert_eq!(s.finish(), None);
    }
}
