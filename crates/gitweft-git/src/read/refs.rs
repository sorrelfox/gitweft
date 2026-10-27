use std::path::Path;

use gix::ObjectId;
use gix::bstr::{BString, ByteSlice};

use super::ReadError;
use crate::cancel::CancelToken;
use crate::cli::GitCli;

/// What HEAD points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadInfo {
    Branch {
        name: String,
        oid: ObjectId,
    },
    Detached {
        oid: ObjectId,
    },
    /// A branch without commits yet (fresh repository or orphan branch).
    Unborn {
        name: String,
    },
}

impl HeadInfo {
    pub fn branch_name(&self) -> Option<&str> {
        match self {
            HeadInfo::Branch { name, .. } | HeadInfo::Unborn { name } => Some(name),
            HeadInfo::Detached { .. } => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefEntry {
    /// Full name, e.g. `refs/heads/main`; kept as bytes because ref names need not be UTF-8.
    pub full_name: BString,
    /// Display name, e.g. `main`, `origin/main`, `v1.0`.
    pub short_name: String,
    /// Commit (or object, for odd tags) the ref ultimately points at.
    pub target: ObjectId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefsModel {
    pub head: HeadInfo,
    pub local_branches: Vec<RefEntry>,
    pub remote_branches: Vec<RefEntry>,
    pub tags: Vec<RefEntry>,
}

pub fn read_refs(repo: &gix::Repository) -> Result<RefsModel, ReadError> {
    let head = read_head(repo)?;
    let platform = repo.references()?;
    Ok(RefsModel {
        head,
        local_branches: collect(platform.local_branches()?)?,
        remote_branches: collect(platform.remote_branches()?)?,
        tags: collect(platform.tags()?)?,
    })
}

fn read_head(repo: &gix::Repository) -> Result<HeadInfo, ReadError> {
    let head = repo.head()?;
    Ok(match head.kind {
        gix::head::Kind::Symbolic(reference) => {
            let name = reference.name.shorten().to_str_lossy().into_owned();
            let oid = reference
                .target
                .try_id()
                .map(ToOwned::to_owned)
                .ok_or("HEAD points at a symbolic ref chain")?;
            HeadInfo::Branch { name, oid }
        }
        gix::head::Kind::Detached { target, .. } => HeadInfo::Detached { oid: target },
        gix::head::Kind::Unborn(name) => HeadInfo::Unborn {
            name: name.shorten().to_str_lossy().into_owned(),
        },
    })
}

fn collect(iter: gix::reference::iter::Iter<'_, '_>) -> Result<Vec<RefEntry>, ReadError> {
    let mut entries = Vec::new();
    for reference in iter {
        let mut reference = reference?;
        // Skip symbolic refs such as `refs/remotes/origin/HEAD`.
        if reference.target().try_id().is_none() {
            continue;
        }
        let target = reference.peel_to_id()?.detach();
        let name = reference.name();
        entries.push(RefEntry {
            full_name: name.as_bstr().to_owned(),
            short_name: name.shorten().to_str_lossy().into_owned(),
            target,
        });
    }
    entries.sort_by(|a, b| a.short_name.cmp(&b.short_name));
    Ok(entries)
}

/// Whether the repository keeps its refs in a reftable (`extensions.refStorage`), which
/// gix 0.87 can't read: its HEAD is a placeholder and its refs aren't files (#17).
pub fn uses_reftable(repo: &gix::Repository) -> bool {
    repo.config_snapshot()
        .string("extensions.refStorage")
        .is_some_and(|value| value.eq_ignore_ascii_case(b"reftable"))
}

/// [`read_refs`], or for a reftable repository [`read_refs_cli`].
pub fn read_refs_with(
    repo: &gix::Repository,
    git: &GitCli,
    cancel: &CancelToken,
) -> Result<RefsModel, ReadError> {
    if uses_reftable(repo) {
        let dir = repo.workdir().unwrap_or_else(|| repo.git_dir());
        return read_refs_cli(git, dir, cancel);
    }
    read_refs(repo)
}

/// The refs and HEAD as git itself reads them: for what gix can't (a reftable). The same
/// model as [`read_refs`]: symbolic refs such as `refs/remotes/origin/HEAD` left out, an
/// annotated tag at the commit it tags.
pub fn read_refs_cli(
    git: &GitCli,
    dir: &Path,
    cancel: &CancelToken,
) -> Result<RefsModel, ReadError> {
    let output = git
        .command(dir)
        .args([
            "for-each-ref",
            "--format=%(refname)%00%(objectname)%00%(*objectname)%00%(symref)",
            "refs/heads",
            "refs/remotes",
            "refs/tags",
        ])
        .read_only()
        .run(cancel)?;
    let (mut local, mut remote, mut tags) = (Vec::new(), Vec::new(), Vec::new());
    for line in output.stdout.split(|&byte| byte == b'\n') {
        let fields: Vec<&[u8]> = line.split(|&byte| byte == 0).collect();
        let [name, target, peeled, symref] = fields[..] else {
            continue;
        };
        if !symref.is_empty() {
            continue;
        }
        let target = if peeled.is_empty() { target } else { peeled };
        let Ok(target) = ObjectId::from_hex(target) else {
            continue;
        };
        let (list, prefix): (&mut Vec<RefEntry>, &[u8]) = if name.starts_with(b"refs/heads/") {
            (&mut local, b"refs/heads/")
        } else if name.starts_with(b"refs/remotes/") {
            (&mut remote, b"refs/remotes/")
        } else {
            (&mut tags, b"refs/tags/")
        };
        list.push(RefEntry {
            full_name: BString::from(name),
            short_name: name[prefix.len()..].to_str_lossy().into_owned(),
            target,
        });
    }
    for list in [&mut local, &mut remote, &mut tags] {
        list.sort_by(|a, b| a.short_name.cmp(&b.short_name));
    }
    Ok(RefsModel {
        head: read_head_cli(git, dir, cancel)?,
        local_branches: local,
        remote_branches: remote,
        tags,
    })
}

/// HEAD from `symbolic-ref` (a branch, born or not; exit 1: detached) and `rev-parse`
/// (its commit; exit 1: unborn).
fn read_head_cli(git: &GitCli, dir: &Path, cancel: &CancelToken) -> Result<HeadInfo, ReadError> {
    let ask = |args: &[&str]| {
        git.command(dir)
            .args(args)
            .allow_exit_codes(&[1])
            .read_only()
            .run(cancel)
    };
    let branch = ask(&["symbolic-ref", "-q", "HEAD"])?;
    let commit = ask(&["rev-parse", "-q", "--verify", "HEAD"])?;
    let name = (branch.status == 0).then(|| {
        let text = branch.stdout_str();
        let full = text.trim();
        full.strip_prefix("refs/heads/").unwrap_or(full).to_owned()
    });
    let oid = match commit.status {
        0 => Some(ObjectId::from_hex(commit.stdout_str().trim().as_bytes())?),
        _ => None,
    };
    Ok(match (name, oid) {
        (Some(name), Some(oid)) => HeadInfo::Branch { name, oid },
        (Some(name), None) => HeadInfo::Unborn { name },
        (None, Some(oid)) => HeadInfo::Detached { oid },
        (None, None) => return Err("HEAD names neither a branch nor a commit".into()),
    })
}
