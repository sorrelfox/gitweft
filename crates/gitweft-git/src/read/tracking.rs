//! Upstream tracking of local branches: which remote branch each one follows and how far
//! apart the two are.

use std::path::Path;

use crate::cancel::CancelToken;
use crate::cli::{GitCli, GitError};

/// The names are text as `undo::ref_name_text` makes it: themselves when UTF-8, else encoded,
/// so a branch whose name isn't UTF-8 still matches its upstream and pushes by its bytes
/// (#63). `undo::ref_name_bytes` gives the bytes back.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tracking {
    /// Local branch name, e.g. `main` or `feature/x`.
    pub branch: String,
    /// The upstream, e.g. `origin/main`.
    pub upstream: Option<String>,
    /// The upstream's remote as configured (`branch.<name>.remote`): `origin`, or `.` for
    /// a local branch (#16).
    pub upstream_remote: Option<String>,
    /// The upstream's branch on that remote (`branch.<name>.merge` without `refs/heads/`),
    /// which the tracking ref's name needn't match.
    pub upstream_branch: Option<String>,
    /// Commits on the branch that the upstream doesn't have.
    pub ahead: u32,
    /// Commits on the upstream that the branch doesn't have.
    pub behind: u32,
    /// The upstream no longer exists (deleted on the remote and pruned).
    pub gone: bool,
}

/// Tracking information for every local branch.
pub fn branch_tracking(
    git: &GitCli,
    workdir: &Path,
    cancel: &CancelToken,
) -> Result<Vec<Tracking>, GitError> {
    let output = git
        .command(workdir)
        .args([
            "for-each-ref",
            // lstrip instead of :short, which abbreviates ambiguous names to `heads/x`.
            "--format=%(refname:lstrip=2)%00%(upstream:lstrip=2)%00%(upstream:track,nobracket)\
             %00%(upstream:remotename)%00%(upstream:remoteref)",
            "refs/heads",
        ])
        .read_only()
        .run(cancel)?;
    Ok(output
        .stdout
        .split(|&b| b == b'\n')
        .filter_map(parse_line)
        .collect())
}

/// `branch NUL upstream NUL track NUL remote NUL merge`, where track is e.g.
/// `ahead 1, behind 2` or `gone`, and merge is the upstream's ref on its remote. The names
/// are taken as bytes (see [`Tracking`]).
fn parse_line(line: &[u8]) -> Option<Tracking> {
    use crate::undo::ref_name_text;
    let mut fields = line.split(|&b| b == 0);
    let branch = ref_name_text(fields.next().filter(|b| !b.is_empty())?);
    let mut field = || fields.next().filter(|f| !f.is_empty());
    let upstream = field().map(ref_name_text);
    let track = field().map(String::from_utf8_lossy).unwrap_or_default();
    let upstream_remote = field().map(ref_name_text);
    let upstream_branch =
        field().map(|merge| ref_name_text(merge.strip_prefix(b"refs/heads/").unwrap_or(merge)));
    let mut tracking = Tracking {
        branch,
        upstream,
        upstream_remote,
        upstream_branch,
        gone: track == "gone",
        ..Tracking::default()
    };
    for part in track.split(", ") {
        if let Some(count) = part.strip_prefix("ahead ") {
            tracking.ahead = count.parse().unwrap_or(0);
        } else if let Some(count) = part.strip_prefix("behind ") {
            tracking.behind = count.parse().unwrap_or(0);
        }
    }
    Some(tracking)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_track_fields() {
        let line = |s: &str| parse_line(s.replace('|', "\0").as_bytes()).unwrap();
        assert_eq!(
            line("main|origin/main|ahead 2, behind 3|origin|refs/heads/main"),
            Tracking {
                branch: "main".into(),
                upstream: Some("origin/main".into()),
                upstream_remote: Some("origin".into()),
                upstream_branch: Some("main".into()),
                ahead: 2,
                behind: 3,
                gone: false,
            }
        );
        assert_eq!(line("feature/x|origin/feature/x|behind 1").behind, 1);
        assert!(line("old|origin/old|gone").gone);
        let local = line("scratch||");
        assert_eq!((local.upstream, local.ahead, local.gone), (None, 0, false));
        // A local upstream (`branch.x.remote = .`), and a branch that pushes elsewhere.
        let on_main = line("feature|main||.|refs/heads/main");
        assert_eq!(on_main.upstream_remote.as_deref(), Some("."));
        assert_eq!(on_main.upstream_branch.as_deref(), Some("main"));
        let renamed = line("renamed|origin/other||origin|refs/heads/other");
        assert_eq!(renamed.upstream_branch.as_deref(), Some("other"));
    }

    /// A branch whose name isn't UTF-8 keeps its bytes, encoded (#63).
    #[test]
    fn names_that_arent_utf8_are_encoded() {
        use crate::undo::{ref_name_bytes, ref_name_text};
        let tracking =
            parse_line(b"caf\xe9\0origin/caf\xe9\0\0origin\0refs/heads/caf\xe9").unwrap();
        assert_eq!(tracking.branch, ref_name_text(b"caf\xe9"));
        assert_eq!(ref_name_bytes(&tracking.branch), b"caf\xe9");
        assert_eq!(tracking.upstream_branch, Some(tracking.branch.clone()));
        assert_eq!(
            tracking.upstream.as_deref().map(ref_name_bytes),
            Some(b"origin/caf\xe9".to_vec())
        );
    }
}
