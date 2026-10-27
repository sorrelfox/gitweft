//! Per-repository settings, kept in `<repo data dir>/settings.json`, and the schedule
//! of the opt-in background fetch. Offline-first: nothing touches the network unless the
//! user turns it on for that repository. Callers run `load`/`save` in a job: file I/O.

use std::io::{self, Read as _, Seek as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gitweft_git::ObjectId;
use gitweft_git::read::{TipFilter, WalkOptions};
use serde::{Deserialize, Serialize};

pub use crate::datafile::Saved;
use crate::paths::{repo_data_dir, repo_data_dir_in};

const VERSION: u32 = 1;
pub const DEFAULT_FETCH_INTERVAL_MINUTES: u32 = 10;

#[derive(Debug, Clone, PartialEq)]
pub struct RepoSettings {
    /// Fetch all remotes periodically, without prompts. Off by default.
    pub background_fetch: bool,
    /// Minutes between background fetches; at least 1.
    pub fetch_interval_minutes: u32,
    /// How the graph walks the history.
    pub walk: WalkOptions,
    /// Refs hidden from the graph's tips, or the one solo ref.
    pub tips: TipFilter,
    /// Commits the user chose to ignore in blame (on top of the repository's
    /// `blame.ignoreRevsFile`).
    pub blame_ignored: Vec<ObjectId>,
    /// The graph column's width as dragged, in design pixels; `None` = sized to the lanes.
    /// Kept here because it depends on the repository's lanes.
    pub graph_width: Option<f32>,
    /// "Not now" on the offer to write a commit-graph file: not offered again.
    pub commit_graph_dismissed: bool,
}

impl Default for RepoSettings {
    fn default() -> Self {
        Self {
            background_fetch: false,
            fetch_interval_minutes: DEFAULT_FETCH_INTERVAL_MINUTES,
            walk: WalkOptions::default(),
            tips: TipFilter::default(),
            blame_ignored: Vec::new(),
            graph_width: None,
            commit_graph_dismissed: false,
        }
    }
}

/// Limits for a remembered graph column width, in design pixels.
pub const GRAPH_WIDTH: (f32, f32) = (40.0, 1600.0);

impl RepoSettings {
    pub fn fetch_interval(&self) -> Duration {
        Duration::from_secs(u64::from(self.fetch_interval_minutes.max(1)) * 60)
    }
}

/// The file as stored: the settings plus a version, so a future format isn't misread.
#[derive(Serialize, Deserialize)]
struct Stored {
    version: u32,
    #[serde(default)]
    background_fetch: bool,
    #[serde(default = "default_interval")]
    fetch_interval_minutes: u32,
    // The graph's walk options (added within version 1: older files lack them).
    #[serde(default)]
    first_parent: bool,
    #[serde(default)]
    current_branch: bool,
    #[serde(default)]
    topo_order: bool,
    // The graph's tip filter (also added within version 1).
    #[serde(default)]
    hidden_refs: Vec<String>,
    #[serde(default)]
    solo_ref: Option<String>,
    // Hex ids of the commits ignored in blame (also added within version 1).
    #[serde(default)]
    blame_ignore_revs: Vec<String>,
    // The graph column's dragged width (also added within version 1).
    #[serde(default)]
    graph_width: Option<f32>,
    // The commit-graph offer dismissed (also added within version 1).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    commit_graph_dismissed: bool,
}

fn default_interval() -> u32 {
    DEFAULT_FETCH_INTERVAL_MINUTES
}

/// The settings file of the repository at `root` (its working tree, or git dir when bare).
pub fn settings_file(root: &Path) -> PathBuf {
    repo_data_dir(root).join("settings.json")
}

/// [`settings_file`] under the data dir `data` (tests pass their own).
pub fn settings_file_in(data: &Path, root: &Path) -> PathBuf {
    repo_data_dir_in(data, root).join("settings.json")
}

/// The settings of the repository at `root` (from [`settings_file`]).
pub fn load(root: &Path) -> RepoSettings {
    load_file(&settings_file(root))
}

/// Saves the settings of the repository at `root` (to [`settings_file`]).
pub fn save(root: &Path, settings: &RepoSettings) -> io::Result<Saved> {
    save_file(&settings_file(root), settings)
}

/// The settings in `file`; the defaults if it's missing, unreadable, or of another
/// version. An interval below one minute becomes one minute.
pub fn load_file(file: &Path) -> RepoSettings {
    let stored = std::fs::read_to_string(file)
        .ok()
        .and_then(|text| serde_json::from_str::<Stored>(&text).ok())
        .filter(|stored| stored.version == VERSION);
    match stored {
        Some(stored) => RepoSettings {
            background_fetch: stored.background_fetch,
            fetch_interval_minutes: stored.fetch_interval_minutes.max(1),
            walk: WalkOptions {
                first_parent: stored.first_parent,
                current_branch: stored.current_branch,
                topo_order: stored.topo_order,
            },
            tips: TipFilter {
                hidden: stored.hidden_refs,
                solo: stored.solo_ref,
            },
            // An entry that isn't an id is dropped.
            blame_ignored: stored
                .blame_ignore_revs
                .iter()
                .filter_map(|hex| ObjectId::from_hex(hex.as_bytes()).ok())
                .collect(),
            graph_width: clamp_graph_width(stored.graph_width),
            commit_graph_dismissed: stored.commit_graph_dismissed,
        },
        None => RepoSettings::default(),
    }
}

impl RepoSettings {
    /// `ours` (this instance's settings) applied onto `theirs` (the file as it is now, maybe
    /// changed by another Gitweft on the same repository) where `base` is what this instance
    /// last read or saved: each setting it changed since wins, every other one is theirs. A
    /// list merges by item: what this instance added or removed since `base` is added to, or
    /// removed from, theirs.
    pub fn merged(base: &Self, ours: &Self, theirs: &Self) -> Self {
        fn pick<T: PartialEq + Clone>(base: &T, ours: &T, theirs: &T) -> T {
            if ours != base {
                ours.clone()
            } else {
                theirs.clone()
            }
        }
        let (b, o, t) = (&base.walk, &ours.walk, &theirs.walk);
        Self {
            background_fetch: pick(
                &base.background_fetch,
                &ours.background_fetch,
                &theirs.background_fetch,
            ),
            fetch_interval_minutes: pick(
                &base.fetch_interval_minutes,
                &ours.fetch_interval_minutes,
                &theirs.fetch_interval_minutes,
            ),
            walk: WalkOptions {
                first_parent: pick(&b.first_parent, &o.first_parent, &t.first_parent),
                current_branch: pick(&b.current_branch, &o.current_branch, &t.current_branch),
                topo_order: pick(&b.topo_order, &o.topo_order, &t.topo_order),
            },
            tips: TipFilter {
                hidden: merge_list(&base.tips.hidden, &ours.tips.hidden, &theirs.tips.hidden),
                solo: pick(&base.tips.solo, &ours.tips.solo, &theirs.tips.solo),
            },
            blame_ignored: merge_list(
                &base.blame_ignored,
                &ours.blame_ignored,
                &theirs.blame_ignored,
            ),
            graph_width: pick(&base.graph_width, &ours.graph_width, &theirs.graph_width),
            commit_graph_dismissed: pick(
                &base.commit_graph_dismissed,
                &ours.commit_graph_dismissed,
                &theirs.commit_graph_dismissed,
            ),
        }
    }
}

/// `theirs`, less what `ours` removed since `base`, plus what it added (in its order).
fn merge_list<T: PartialEq + Clone>(base: &[T], ours: &[T], theirs: &[T]) -> Vec<T> {
    let mut merged: Vec<T> = theirs
        .iter()
        .filter(|item| ours.contains(item) || !base.contains(item))
        .cloned()
        .collect();
    for item in ours {
        if !base.contains(item) && !merged.contains(item) {
            merged.push(item.clone());
        }
    }
    merged
}

/// Saves `ours` for the repository at `root` without undoing another instance's changes:
/// holding a lock (`settings.json.lock`), the file as it is now is read, and
/// [`RepoSettings::merged`] with `base` (what this instance last read or saved) is written.
/// Returns what was written.
pub fn save_merged(
    root: &Path,
    base: &RepoSettings,
    ours: &RepoSettings,
) -> io::Result<(RepoSettings, Saved)> {
    save_merged_file(&settings_file(root), base, ours)
}

/// [`save_merged`] for a settings file given by path.
pub fn save_merged_file(
    file: &Path,
    base: &RepoSettings,
    ours: &RepoSettings,
) -> io::Result<(RepoSettings, Saved)> {
    crate::datafile::with_lock(file, || {
        let merged = RepoSettings::merged(base, ours, &load_file(file));
        let saved = save_file(file, &merged)?;
        Ok((merged, saved))
    })
}

/// A graph width within [`GRAPH_WIDTH`]; not a number is none.
pub fn clamp_graph_width(width: Option<f32>) -> Option<f32> {
    width
        .filter(|width| width.is_finite())
        .map(|width| width.clamp(GRAPH_WIDTH.0, GRAPH_WIDTH.1))
}

/// Writes the settings through [`crate::datafile::write_json`]: a file this build can't
/// read is kept as a backup first, and one written by a newer version is left alone (so
/// defaults read from it never replace it).
pub fn save_file(file: &Path, settings: &RepoSettings) -> io::Result<Saved> {
    let stored = Stored {
        version: VERSION,
        background_fetch: settings.background_fetch,
        fetch_interval_minutes: settings.fetch_interval_minutes.max(1),
        first_parent: settings.walk.first_parent,
        current_branch: settings.walk.current_branch,
        topo_order: settings.walk.topo_order,
        hidden_refs: settings.tips.hidden.clone(),
        solo_ref: settings.tips.solo.clone(),
        blame_ignore_revs: settings
            .blame_ignored
            .iter()
            .map(ToString::to_string)
            .collect(),
        graph_width: clamp_graph_width(settings.graph_width),
        commit_graph_dismissed: settings.commit_graph_dismissed,
    };
    let text = serde_json::to_string_pretty(&stored).map_err(io::Error::other)?;
    crate::datafile::write_json(file, u64::from(VERSION), &text)
}

/// Whether the next background fetch is due: `interval` has passed since `last`, the
/// start of the previous attempt (successful or not, so a failing remote is retried at
/// the same pace rather than in a loop). Before the first attempt, pass the time the
/// repository was opened: the first fetch comes one interval after opening, not at
/// once, so opening a repository never touches the network by itself. `None` (not
/// opened yet, nothing to count from) is never due.
pub fn fetch_due(last: Option<Instant>, now: Instant, interval: Duration) -> bool {
    last.is_some_and(|last| now.saturating_duration_since(last) >= interval)
}

/// The file in a repository's data dir that holds the start of its last background fetch.
const FETCH_CLAIM: &str = "fetch-claim";

/// Claims this interval's background fetch of the repository at `root`: every tab
/// and every Gitweft on it has its own timer, and only the one that gets the claim fetches.
/// False when another started one within this interval. Run it in a job: file I/O.
pub fn claim_fetch(root: &Path, interval: Duration) -> io::Result<bool> {
    claim_fetch_in(&repo_data_dir(root), interval, SystemTime::now())
}

/// [`claim_fetch`] in the data dir `dir` at `now`. The claim is the time, in seconds, of
/// the last fetch started, read and replaced under an exclusive lock on the file, so two
/// processes can't both take it. A crashed holder's lock goes with its process, and its
/// claim simply runs out. A claim from the future (the clock was set back) doesn't count.
pub fn claim_fetch_in(dir: &Path, interval: Duration, now: SystemTime) -> io::Result<bool> {
    std::fs::create_dir_all(dir)?;
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join(FETCH_CLAIM))?;
    file.lock()?;
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    let now = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    // Nine tenths of an interval: a tab's own timer starts its next fetch one interval
    // after its last, and mustn't be refused by its own claim.
    let window = interval.mul_f64(0.9).as_secs();
    let taken = text
        .trim()
        .parse::<u64>()
        .is_ok_and(|last| last <= now && now - last < window);
    if taken {
        return Ok(false);
    }
    file.set_len(0)?;
    file.rewind()?;
    writeln!(file, "{now}")?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_claim_per_interval() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("repos/app");
        let interval = Duration::from_secs(600);
        let at = |secs: u64| UNIX_EPOCH + Duration::from_secs(1_800_000_000 + secs);

        assert!(
            claim_fetch_in(&data, interval, at(0)).unwrap(),
            "the first claims"
        );
        assert!(
            !claim_fetch_in(&data, interval, at(5)).unwrap(),
            "another tab, 5 s later"
        );
        assert!(!claim_fetch_in(&data, interval, at(539)).unwrap());
        // The claimer's own next fetch, one interval on (or a little early).
        assert!(claim_fetch_in(&data, interval, at(598)).unwrap());
        assert!(!claim_fetch_in(&data, interval, at(600)).unwrap());
        // A claim from the future (clock set back) or an unreadable one doesn't hold.
        assert!(claim_fetch_in(&data, interval, at(100)).unwrap());
        std::fs::write(data.join(FETCH_CLAIM), "junk").unwrap();
        assert!(claim_fetch_in(&data, interval, at(101)).unwrap());
    }

    #[test]
    fn claimers_at_the_same_moment_get_one_claim() {
        let dir = tempfile::tempdir().unwrap();
        let data = std::sync::Arc::new(dir.path().join("repos/app"));
        let now = SystemTime::now();
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let data = data.clone();
                std::thread::spawn(move || {
                    claim_fetch_in(&data, Duration::from_secs(600), now).unwrap()
                })
            })
            .collect();
        let won = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .filter(|&won| won)
            .count();
        assert_eq!(won, 1);
    }

    #[test]
    fn defaults_are_offline() {
        let settings = RepoSettings::default();
        assert!(!settings.background_fetch);
        assert_eq!(settings.fetch_interval(), Duration::from_secs(600));
    }

    #[test]
    fn settings_round_trip_and_bad_files_give_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("repos/app/settings.json");
        assert_eq!(load_file(&file), RepoSettings::default());
        let settings = RepoSettings {
            background_fetch: true,
            fetch_interval_minutes: 30,
            walk: WalkOptions {
                first_parent: true,
                current_branch: false,
                topo_order: true,
            },
            tips: TipFilter {
                hidden: vec!["refs/heads/wip".into()],
                solo: None,
            },
            blame_ignored: vec![ObjectId::from_hex(&[b'a'; 40]).unwrap()],
            graph_width: Some(180.0),
            // Dismissed stays dismissed across loads.
            commit_graph_dismissed: true,
        };
        save_file(&file, &settings).unwrap();
        assert_eq!(load_file(&file), settings);
        assert!(!file.with_extension("tmp").exists());

        let text = std::fs::read_to_string(&file).unwrap();
        let newer = text.replace("\"version\": 1", "\"version\": 2");
        std::fs::write(&file, &newer).unwrap();
        assert_eq!(load_file(&file), RepoSettings::default());
        // The defaults read from a newer version's file never replace it.
        assert_eq!(save_file(&file, &settings).unwrap(), Saved::NewerKept);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), newer);

        std::fs::write(&file, "{ not json").unwrap();
        assert_eq!(load_file(&file), RepoSettings::default());
        // An unreadable one is kept aside before the next save replaces it.
        let backup = dir.path().join("repos/app/settings.json.bak");
        assert_eq!(
            save_file(&file, &settings).unwrap(),
            Saved::SetAside(backup.clone())
        );
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), "{ not json");
        assert_eq!(load_file(&file), settings);
    }

    /// Two instances on one repository, each saving from what it read: neither undoes the
    /// other's settings.
    #[test]
    fn two_instances_keep_each_others_settings() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("settings.json");
        let base = RepoSettings::default();
        let hidden = |refs: &[&str]| TipFilter {
            hidden: refs.iter().map(|r| r.to_string()).collect(),
            solo: None,
        };
        // One hides a branch and widens the graph…
        let a = RepoSettings {
            tips: hidden(&["refs/heads/a"]),
            graph_width: Some(300.0),
            ..base.clone()
        };
        save_merged_file(&file, &base, &a).unwrap();
        // …the other, from the same start, walks first parents and hides another branch.
        let b = RepoSettings {
            walk: WalkOptions {
                first_parent: true,
                ..WalkOptions::default()
            },
            tips: hidden(&["refs/heads/b"]),
            ..base.clone()
        };
        let (merged, saved) = save_merged_file(&file, &base, &b).unwrap();
        assert_eq!(saved, Saved::Written);
        assert!(merged.walk.first_parent);
        assert_eq!(merged.graph_width, Some(300.0));
        assert_eq!(merged.tips, hidden(&["refs/heads/a", "refs/heads/b"]));
        assert_eq!(load_file(&file), merged);
        // The first shows its branch again: the other's stays hidden, its walk stays.
        let a2 = RepoSettings {
            tips: TipFilter::default(),
            ..a.clone()
        };
        let (merged, _) = save_merged_file(&file, &a, &a2).unwrap();
        assert_eq!(merged.tips, hidden(&["refs/heads/b"]));
        assert!(merged.walk.first_parent);
        assert_eq!(merged.graph_width, Some(300.0));
    }

    #[test]
    fn missing_fields_default_and_the_interval_is_at_least_a_minute() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("settings.json");
        std::fs::write(&file, r#"{"version": 1, "background_fetch": true}"#).unwrap();
        assert_eq!(
            load_file(&file),
            RepoSettings {
                background_fetch: true,
                fetch_interval_minutes: DEFAULT_FETCH_INTERVAL_MINUTES,
                walk: WalkOptions::default(),
                tips: TipFilter::default(),
                blame_ignored: Vec::new(),
                graph_width: None,
                commit_graph_dismissed: false,
            }
        );
        std::fs::write(&file, r#"{"version": 1, "fetch_interval_minutes": 0}"#).unwrap();
        assert_eq!(load_file(&file).fetch_interval_minutes, 1);
        let zero = RepoSettings {
            fetch_interval_minutes: 0,
            ..RepoSettings::default()
        };
        assert_eq!(zero.fetch_interval(), Duration::from_secs(60));
    }

    #[test]
    fn walk_options_round_trip_and_an_older_file_walks_everything() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("settings.json");
        for walk in [
            WalkOptions {
                first_parent: true,
                ..WalkOptions::default()
            },
            WalkOptions {
                current_branch: true,
                ..WalkOptions::default()
            },
            WalkOptions {
                topo_order: true,
                ..WalkOptions::default()
            },
        ] {
            let settings = RepoSettings {
                walk,
                ..RepoSettings::default()
            };
            save_file(&file, &settings).unwrap();
            assert_eq!(load_file(&file), settings);
        }
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains("\"topo_order\": true"), "{text}");

        // A version-1 file written before the walk options existed.
        std::fs::write(
            &file,
            r#"{"version": 1, "background_fetch": true, "fetch_interval_minutes": 5}"#,
        )
        .unwrap();
        let loaded = load_file(&file);
        assert_eq!(loaded.walk, WalkOptions::default());
        assert!(loaded.background_fetch);
        assert_eq!(loaded.fetch_interval_minutes, 5);
    }

    #[test]
    fn the_tip_filter_round_trips_and_an_older_file_hides_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("settings.json");
        for tips in [
            TipFilter {
                hidden: vec!["refs/remotes/origin/".into(), "refs/tags/v1".into()],
                solo: None,
            },
            TipFilter {
                hidden: Vec::new(),
                solo: Some("refs/heads/feature".into()),
            },
            TipFilter::default(),
        ] {
            let settings = RepoSettings {
                tips,
                ..RepoSettings::default()
            };
            save_file(&file, &settings).unwrap();
            assert_eq!(load_file(&file), settings);
        }
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains("\"hidden_refs\": []"), "{text}");
        assert!(text.contains("\"solo_ref\": null"), "{text}");

        std::fs::write(
            &file,
            r#"{"version": 1, "hidden_refs": ["refs/heads/a"], "solo_ref": "refs/heads/b"}"#,
        )
        .unwrap();
        assert_eq!(
            load_file(&file).tips,
            TipFilter {
                hidden: vec!["refs/heads/a".into()],
                solo: Some("refs/heads/b".into()),
            }
        );

        // A version-1 file written before the tip filter existed.
        std::fs::write(&file, r#"{"version": 1, "first_parent": true}"#).unwrap();
        let loaded = load_file(&file);
        assert!(loaded.tips.is_empty());
        assert!(loaded.walk.first_parent);
    }

    #[test]
    fn ignored_blame_commits_round_trip_and_an_older_file_ignores_none() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("settings.json");
        let a = ObjectId::from_hex(&[b'1'; 40]).unwrap();
        let b = ObjectId::from_hex(&[b'2'; 40]).unwrap();
        for blame_ignored in [vec![a, b], Vec::new()] {
            let settings = RepoSettings {
                blame_ignored,
                ..RepoSettings::default()
            };
            save_file(&file, &settings).unwrap();
            assert_eq!(load_file(&file), settings);
        }
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains("\"blame_ignore_revs\": []"), "{text}");

        // Entries that aren't ids are dropped; the rest keep their order.
        std::fs::write(
            &file,
            format!(r#"{{"version": 1, "blame_ignore_revs": ["{b}", "not an id", "abc", "{a}"]}}"#),
        )
        .unwrap();
        assert_eq!(load_file(&file).blame_ignored, [b, a]);

        // A version-1 file written before ignored blame commits existed.
        std::fs::write(&file, r#"{"version": 1, "topo_order": true}"#).unwrap();
        let loaded = load_file(&file);
        assert!(loaded.blame_ignored.is_empty());
        assert!(loaded.walk.topo_order);
    }

    #[test]
    fn the_first_fetch_comes_one_interval_after_opening() {
        let opened = Instant::now();
        let interval = Duration::from_secs(600);
        assert!(!fetch_due(Some(opened), opened, interval));
        let almost = opened + Duration::from_secs(599);
        assert!(!fetch_due(Some(opened), almost, interval));
        assert!(fetch_due(Some(opened), opened + interval, interval));
        // A clock reading before `last` (it shouldn't happen) is simply not due.
        assert!(!fetch_due(Some(opened + interval), opened, interval));
        // Nothing to count from yet: not due.
        assert!(!fetch_due(None, opened + interval, interval));
    }

    #[test]
    fn one_file_per_repository() {
        let a = settings_file(Path::new("/home/u/code/app"));
        assert_ne!(a, settings_file(Path::new("/home/u/other/app")));
        assert_eq!(a.file_name().unwrap(), "settings.json");
    }
}
