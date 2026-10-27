use std::fmt;

/// A parsed `git version` result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GitVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl GitVersion {
    /// Oldest git we support (Debian 12 ships 2.39).
    pub const MINIMUM: GitVersion = GitVersion::new(2, 39, 0);

    pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    /// Parses output such as `git version 2.55.0`, `git version 2.45.0.rc1`
    /// or `git version 2.39.5 (Apple Git-154)`.
    pub fn parse(output: &str) -> Option<Self> {
        let rest = output.trim().strip_prefix("git version ")?;
        let token = rest.split_whitespace().next()?;
        let mut parts = token.split('.').map(|part| {
            let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
            digits.parse::<u32>().ok()
        });
        let major = parts.next()??;
        let minor = parts.next()??;
        let patch = parts.next().flatten().unwrap_or(0);
        Some(Self::new(major, minor, patch))
    }

    pub fn is_supported(&self) -> bool {
        *self >= Self::MINIMUM
    }
}

impl fmt::Display for GitVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_formats() {
        assert_eq!(
            GitVersion::parse("git version 2.55.0\n"),
            Some(GitVersion::new(2, 55, 0))
        );
        assert_eq!(
            GitVersion::parse("git version 2.45.0.rc1"),
            Some(GitVersion::new(2, 45, 0))
        );
        assert_eq!(
            GitVersion::parse("git version 2.39.5 (Apple Git-154)"),
            Some(GitVersion::new(2, 39, 5))
        );
        assert_eq!(
            GitVersion::parse("git version 2.40"),
            Some(GitVersion::new(2, 40, 0))
        );
        assert_eq!(GitVersion::parse("hg version 6"), None);
    }

    #[test]
    fn minimum_gate() {
        assert!(GitVersion::new(2, 39, 0).is_supported());
        assert!(!GitVersion::new(2, 38, 9).is_supported());
        assert!(GitVersion::new(3, 0, 0).is_supported());
    }
}
