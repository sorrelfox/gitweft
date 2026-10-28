//! A repository's own id, `gitweft.id` in its `.git/config`: Gitweft's files about the
//! repository (undo log, reviews, settings, conflict record) are kept under it rather than
//! under a hash of its path, so they follow the repository when it's moved or renamed.
//!
//! The id is written once, when the app first loads the repository
//! ([`ensure_repo_id_in`]), and read back through [`repo_key`]. Until then (or where the
//! config can't be written) the folder is the one named after the path, as before.
//! Linked worktrees share the repository's config, so each gets the id plus its own
//! worktree name: their undo logs stay apart.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use gitweft_git::{CancelToken, GitCli, gix};
use gitweft_platform::bytes::path_from_bytes;

use crate::paths::{file_name_part, fnv1a, legacy_repo_data_dir_in};

/// The config key.
const KEY: &str = "gitweft.id";

/// What each repository root was found to be keyed by: its folder name under `repos/`,
/// or `None` for one without an id (then the path-named folder).
static KEYS: Mutex<Option<HashMap<PathBuf, Option<String>>>> = Mutex::new(None);

/// The folder name under `<data dir>/repos/` for the repository at `root`, when it has an
/// id: the id, plus `-wt-<name>` in a linked worktree. `None` without one (or when `root`
/// isn't a repository). Read from the repository's config once per root and process.
pub fn repo_key(root: &Path) -> Option<String> {
    let mut keys = KEYS.lock().unwrap_or_else(|poison| poison.into_inner());
    let keys = keys.get_or_insert_with(HashMap::new);
    keys.entry(root.to_owned())
        .or_insert_with(|| read_key(root))
        .clone()
}

fn remember(root: &Path, key: Option<String>) {
    let mut keys = KEYS.lock().unwrap_or_else(|poison| poison.into_inner());
    keys.get_or_insert_with(HashMap::new)
        .insert(root.to_owned(), key);
}

/// The repository at `root` (no discovery upwards: `root` is its top), with its own config
/// only.
fn open(root: &Path) -> Option<gix::Repository> {
    gix::open_opts(root, gix::open::Options::isolated()).ok()
}

fn read_key(root: &Path) -> Option<String> {
    let repo = open(root)?;
    let id = repo.config_snapshot().string(KEY)?.to_string();
    Some(key_for(&repo, valid_id(&id)?))
}

/// `id` if it's safe as a folder name: letters, digits, `.`, `_` and `-`, not starting
/// with `.`, at most 100 characters. Anything else in the config (edited by hand, or
/// hostile) is ignored.
fn valid_id(id: &str) -> Option<&str> {
    let ok = !id.is_empty()
        && id.len() <= 100
        && !id.starts_with('.')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    ok.then_some(id)
}

/// The id, and in a linked worktree its name (the folder under `.git/worktrees/`, which
/// `git worktree move` keeps).
fn key_for(repo: &gix::Repository, id: &str) -> String {
    if repo.git_dir() == repo.common_dir() {
        return id.to_owned();
    }
    let name = repo
        .git_dir()
        .file_name()
        .map(|name| file_name_part(&name.to_string_lossy()))
        .unwrap_or_default();
    format!("{id}-wt-{name}")
}

/// A new id: the folder's name in ASCII (readable in the data dir) and 16 hex digits from
/// the path, the time and the process.
fn new_id(root: &Path) -> String {
    let name: String = root
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .take(40)
        .collect();
    let name = if name.is_empty() || name.starts_with('.') {
        "repo".to_owned()
    } else {
        name
    };
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let seed = format!("{}\0{nanos}\0{}", root.display(), std::process::id());
    format!("{name}-{:016x}", fnv1a(seed.as_bytes()))
}

/// The file in a repository's folder naming the path it was last loaded from: how a copy
/// (same id, and the original still there) is told from a move.
const ROOT_FILE: &str = "root";

/// The paths a repository was loaded from before it was moved, one per line.
const PREVIOUS_ROOTS_FILE: &str = "previous-roots";

fn read_lines(file: &Path) -> Vec<PathBuf> {
    std::fs::read(file)
        .unwrap_or_default()
        .split(|&b| b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| path_from_bytes(line).into_owned())
        .collect()
}

/// Where the repository at `root` was before it was moved (as far as Gitweft saw it
/// loaded there), under the data dir `data`.
pub fn previous_roots_in(data: &Path, root: &Path) -> Vec<PathBuf> {
    match repo_key(root) {
        Some(key) => read_lines(&data.join("repos").join(key).join(PREVIOUS_ROOTS_FILE)),
        None => Vec::new(),
    }
}

/// Where the repository that was at `old` is now, if Gitweft saw it moved (under the data
/// dir `data`): for records that name the old path, such as trash entries.
pub fn moved_to_in(data: &Path, old: &Path) -> Option<PathBuf> {
    let folders = std::fs::read_dir(data.join("repos")).ok()?;
    folders.flatten().find_map(|folder| {
        let previous = read_lines(&folder.path().join(PREVIOUS_ROOTS_FILE));
        if !previous.iter().any(|path| path == old) {
            return None;
        }
        let now = std::fs::read(folder.path().join(ROOT_FILE)).ok()?;
        let now = path_from_bytes(&now).into_owned();
        now.is_dir().then_some(now)
    })
}

/// Writes a new id into the repository's config; its key, or `None` if git can't.
fn assign(
    git: &GitCli,
    repo: &gix::Repository,
    root: &Path,
    cancel: &CancelToken,
) -> Option<String> {
    let id = new_id(root);
    git.command(root)
        .args(["config", KEY, &id])
        .run(cancel)
        .ok()?;
    Some(key_for(repo, &id))
}

/// Makes sure the repository at `root` has an id, writing one into its config if it has
/// none (`git config gitweft.id`), and moves its files from the path-named folder under
/// `data` to the id's if they are still there. A copy of a repository (its `.git` copied
/// along, so the same id, while the original is still where it was) gets an id of its own.
/// Returns the folder name, or `None` when the config can't be written (a read-only
/// repository): the path-named folder stays in use.
pub fn ensure_repo_id_in(
    data: &Path,
    git: &GitCli,
    root: &Path,
    cancel: &CancelToken,
) -> Option<String> {
    let repo = open(root)?;
    let mut key = match read_key(root) {
        Some(key) => key,
        None => assign(git, &repo, root, cancel)?,
    };
    let recorded = std::fs::read_to_string(data.join("repos").join(&key).join(ROOT_FILE))
        .ok()
        .map(PathBuf::from);
    let copied = recorded.as_ref().is_some_and(|other| {
        other != root && other.exists() && read_key(other).as_ref() == Some(&key)
    });
    if copied {
        key = assign(git, &repo, root, cancel)?;
    } else if let Some(old) = recorded.filter(|old| old != root) {
        // Moved: the trash finds entries recorded under the old path through this.
        let file = data.join("repos").join(&key).join(PREVIOUS_ROOTS_FILE);
        let mut previous = read_lines(&file);
        if !previous.contains(&old) {
            previous.push(old);
            let text: Vec<u8> = previous
                .iter()
                .flat_map(|path| [path.as_os_str().as_encoded_bytes(), b"\n"].concat())
                .collect();
            let _ = std::fs::write(&file, text);
        }
    }
    let legacy = legacy_repo_data_dir_in(data, root);
    let keyed = data.join("repos").join(&key);
    if legacy.is_dir() && !keyed.exists() {
        // A failed move keeps the old folder; the new one starts empty.
        let _ = std::fs::rename(&legacy, &keyed);
    }
    if std::fs::create_dir_all(&keyed).is_ok() {
        let _ = std::fs::write(keyed.join(ROOT_FILE), root.as_os_str().as_encoded_bytes());
    }
    remember(root, Some(key.clone()));
    Some(key)
}

#[cfg(test)]
mod tests {
    use super::{new_id, valid_id};

    #[test]
    fn only_plain_names_are_used_as_folders() {
        for good in ["myrepo-3f9a0c1d2e4b5a6f", "a", "x.y_z-1"] {
            assert_eq!(valid_id(good), Some(good));
        }
        for bad in [
            "",
            "..",
            ".hidden",
            "a/b",
            "../../etc",
            "a b",
            "é",
            &"x".repeat(101),
        ] {
            assert_eq!(valid_id(bad), None, "{bad:?}");
        }
        // New ids always pass, whatever the folder is called.
        for folder in ["/r/café", "/r/.hidden", "/", "/r/my repo", "/r/проект"] {
            let id = new_id(std::path::Path::new(folder));
            assert_eq!(valid_id(&id), Some(id.as_str()), "{folder}");
        }
    }
}
