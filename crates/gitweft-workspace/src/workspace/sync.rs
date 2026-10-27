//! Git LFS and shallow-clone banners, and the opt-in background fetch.

use std::time::{Duration, Instant, SystemTime};

use gitweft_core::repo_settings::{self, RepoSettings};
use gitweft_git::lfs::{LfsStatus, lfs_files, lfs_status};
use gitweft_git::read::shallow_commits;
use gitweft_git::remote::{Deepen, fetch_background};

use super::*;

/// How often the background-fetch timer looks at the clock. It does no I/O unless a
/// fetch is due.
const TICK: Duration = Duration::from_secs(5);
/// Commits fetched by "Fetch more history".
pub(super) const DEEPEN_BY: u32 = 100;

/// The background-fetch intervals the "More" menu offers, in minutes.
pub(super) const FETCH_INTERVALS: [u32; 5] = [5, 10, 15, 30, 60];

/// The action that sets `minutes` (one of [`FETCH_INTERVALS`]) as the interval.
pub(super) fn fetch_every_action(minutes: u32) -> Box<dyn Action> {
    match minutes {
        5 => Box::new(FetchEvery5),
        15 => Box::new(FetchEvery15),
        30 => Box::new(FetchEvery30),
        60 => Box::new(FetchEvery60),
        _ => Box::new(FetchEvery10),
    }
}

/// The toast for a data file a save couldn't simply replace: kept aside because this
/// build couldn't read it, or left alone because a newer version of Gitweft wrote it.
pub(super) fn data_notice(file: &std::path::Path, saved: &repo_settings::Saved) -> Option<Toast> {
    let shown = file.display().to_string();
    let text = match saved {
        repo_settings::Saved::Written => return None,
        repo_settings::Saved::SetAside(backup) => t!(
            "data.set_aside",
            file = shown,
            backup = backup.display().to_string()
        ),
        repo_settings::Saved::NewerKept => t!("data.newer_kept", file = shown),
    };
    Some(Toast::warning(text.into_owned()))
}

/// What a load found out about Git LFS and a shallow history.
#[derive(Debug, Default, Clone, PartialEq)]
pub(super) struct RepoExtras {
    /// Set when files in the index have `filter=lfs`.
    pub lfs: Option<LfsStatus>,
    /// LFS files that are still pointer files in the working tree.
    pub lfs_missing: usize,
    /// Where a shallow clone's history stops (empty when it's complete).
    pub shallow: Vec<ObjectId>,
    /// When the remotes were last fetched: `FETCH_HEAD`'s time (every fetch and pull,
    /// from the app or a terminal, rewrites it).
    pub fetched_at: Option<SystemTime>,
}

/// One tab's banners and background fetch.
#[derive(Default)]
pub(super) struct SyncState {
    pub extras: RepoExtras,
    pub lfs_dismissed: bool,
    pub shallow_dismissed: bool,
    pub settings: RepoSettings,
    /// The settings as this tab last read or saved them: a save merges the tab's changes
    /// since onto the file as another Gitweft on the repository may have left it.
    pub settings_base: RepoSettings,
    /// The repository whose settings are loaded (and saved to).
    pub settings_root: Option<PathBuf>,
    /// When the last background fetch started, or when the repository was opened.
    pub last_fetch: Option<Instant>,
    /// Cancels the background fetch that is running.
    pub fetch_running: Option<CancelToken>,
    /// Why the last background fetch failed, until one succeeds.
    pub fetch_error: Option<String>,
    pub timer: Option<Task<()>>,
    /// Timer ticks, to redraw "fetched N min ago" every 30 s.
    pub ticks: u32,
}

impl SyncState {
    /// Another repository opens in the tab: forget what belonged to the last one.
    pub fn reset(&mut self) {
        if let Some(cancel) = self.fetch_running.take() {
            cancel.cancel();
        }
        self.extras = RepoExtras::default();
        self.lfs_dismissed = false;
        self.shallow_dismissed = false;
        self.settings_root = None;
        self.fetch_error = None;
    }

    /// LFS is used and git-lfs is installed: files can be locked on the LFS server.
    pub fn lfs_locking(&self) -> bool {
        self.extras
            .lfs
            .as_ref()
            .is_some_and(|lfs| lfs.version.is_some())
    }

    /// LFS is used and git-lfs can download: the LFS menu items apply.
    pub fn lfs_ready(&self) -> bool {
        self.extras
            .lfs
            .as_ref()
            .is_some_and(|lfs| lfs.version.is_some() && lfs.filter_configured)
    }
}

impl Workspace {
    fn repo_cwd(&self) -> Option<(GitCli, PathBuf)> {
        let RepoState::Loaded(repo) = &self.state else {
            return None;
        };
        let cwd = repo.cwd();
        Some((repo.git.clone(), cwd))
    }

    /// Reads LFS use and the shallow boundary: after every load, and after LFS and
    /// deepening operations.
    pub(super) fn load_extras(&mut self, cx: &mut Context<Self>) {
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let shared = repo.repo.clone();
        let bare = repo.workdir.is_none();
        let Some((git, cwd)) = self.repo_cwd() else {
            return;
        };
        let job = self.jobs.spawn(Lane::Coalesce("extras"), move |cancel| {
            let shallow = shallow_commits(&shared.to_thread_local()).unwrap_or_default();
            let lfs = if bare {
                None
            } else {
                lfs_status(&git, &cwd, cancel).ok().filter(|lfs| lfs.used)
            };
            let lfs_missing = match &lfs {
                Some(lfs) if lfs.version.is_some() => lfs_files(&git, &cwd, cancel)
                    .map(|files| files.iter().filter(|file| !file.downloaded).count())
                    .unwrap_or(0),
                _ => 0,
            };
            let fetched_at =
                std::fs::metadata(shared.to_thread_local().git_dir().join("FETCH_HEAD"))
                    .and_then(|meta| meta.modified())
                    .ok();
            RepoExtras {
                lfs,
                lfs_missing,
                shallow,
                fetched_at,
            }
        });
        cx.spawn(async move |this, cx| {
            if let Some(extras) = job.join().await {
                this.update(cx, |this, cx| {
                    // Every time: a reload can bring a new graph view.
                    if let Some(graph) = this.graph_view.clone() {
                        let boundary = extras.shallow.clone();
                        graph.update(cx, |graph, cx| graph.set_shallow(boundary, cx));
                    }
                    if this.sync.extras != extras {
                        this.sync.extras = extras;
                        let locking = this.sync.lfs_locking();
                        this.staging_view
                            .update(cx, |staging, cx| staging.set_lfs_locks(locking, cx));
                        cx.notify();
                    }
                })
                .ok();
            }
        })
        .detach();
    }

    /// Reads the repository's settings (once per repository) and starts the timer.
    pub(super) fn load_settings(&mut self, root: &Path, cx: &mut Context<Self>) {
        if self.sync.settings_root.as_deref() == Some(root) {
            return;
        }
        let root = root.to_owned();
        self.sync.settings_root = Some(root.clone());
        self.sync.settings = RepoSettings::default();
        self.sync.settings_base = RepoSettings::default();
        // The first background fetch comes one interval after opening.
        self.sync.last_fetch = Some(Instant::now());
        // The write lane: a save queued after this read waits for it.
        let job = self.jobs.spawn(Lane::Write, {
            let root = root.clone();
            move |_| repo_settings::load(&root)
        });
        cx.spawn(async move |this, cx| {
            let Some(settings) = job.join().await else {
                return;
            };
            this.update(cx, |this, cx| {
                if this.sync.settings_root.as_ref() == Some(&root) {
                    // The graph's walk is what the load read, or what was chosen since.
                    let walk = this.walk.unwrap_or(settings.walk);
                    let tips = this.tips.clone().unwrap_or(settings.tips.clone());
                    this.sync.settings_base = settings.clone();
                    this.sync.settings = RepoSettings {
                        walk,
                        tips,
                        ..settings
                    };
                    // Dismissed before: the first load's offer goes.
                    if this.sync.settings.commit_graph_dismissed
                        && this.commit_graph_banner == CommitGraphBanner::Offer
                    {
                        this.commit_graph_banner = CommitGraphBanner::Hidden;
                    }
                    // The first graph was built before this read.
                    let width = this.sync.settings.graph_width;
                    if let Some(view) = &this.graph_view {
                        view.update(cx, |view, cx| view.set_graph_width(width, cx));
                    }
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
        self.start_fetch_timer(cx);
    }

    fn start_fetch_timer(&mut self, cx: &mut Context<Self>) {
        if self.sync.timer.is_some() {
            return;
        }
        self.sync.timer = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(TICK).await;
                if this
                    .update(cx, |this, cx| this.background_fetch_tick(cx))
                    .is_err()
                {
                    break;
                }
            }
        }));
    }

    fn fetch_interval(&self) -> Duration {
        // Debug builds: a short interval for click-tests.
        #[cfg(debug_assertions)]
        if let Some(secs) = std::env::var("GITWEFT_DEBUG_FETCH_INTERVAL_SECS")
            .ok()
            .and_then(|secs| secs.parse().ok())
        {
            return Duration::from_secs(secs);
        }
        self.sync.settings.fetch_interval()
    }

    fn background_fetch_tick(&mut self, cx: &mut Context<Self>) {
        self.sync.ticks = self.sync.ticks.wrapping_add(1);
        if self.visible
            && self.sync.ticks.is_multiple_of(6)
            && self.sync.extras.fetched_at.is_some()
        {
            cx.notify();
        }
        let idle = self.sync.fetch_running.is_none() && self.remote_op.is_none();
        if self.sync.settings.background_fetch
            && idle
            && !self.remotes.is_empty()
            && repo_settings::fetch_due(self.sync.last_fetch, Instant::now(), self.fetch_interval())
        {
            self.start_background_fetch(cx);
        }
    }

    /// Fetches every remote without asking for credentials. A failure only shows as a
    /// badge next to Fetch. Another tab or Gitweft on the repository that fetched within
    /// this interval makes it skip: the watcher shows what that one fetched.
    fn start_background_fetch(&mut self, cx: &mut Context<Self>) {
        let Some((git, cwd)) = self.repo_cwd() else {
            return;
        };
        let (root, interval) = (self.sync.settings_root.clone(), self.fetch_interval());
        let job = self.jobs.spawn(Lane::Write, move |cancel| {
            // A data dir that can't be written doesn't stop the fetch.
            let claimed =
                root.is_none_or(|root| repo_settings::claim_fetch(&root, interval).unwrap_or(true));
            if !claimed {
                return Ok(None);
            }
            fetch_background(&git, &cwd, cancel).map(Some)
        });
        self.sync.last_fetch = Some(Instant::now());
        self.sync.fetch_running = Some(job.cancel_token().clone());
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = job.join().await;
            this.update(cx, |this, cx| {
                this.sync.fetch_running = None;
                match result {
                    Some(Ok(None)) => {}
                    Some(Ok(Some(_))) => {
                        this.sync.fetch_error = None;
                        this.refresh_refs(cx);
                        // FETCH_HEAD's new time, for the status bar.
                        this.load_extras(cx);
                    }
                    Some(Err(err)) if err.kind == GitErrorKind::Cancelled => {}
                    Some(Err(err)) => this.sync.fetch_error = Some(err.summary()),
                    None => {}
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Saves the repository's settings as they are now, in the write lane (after the read
    /// and any earlier save).
    pub(super) fn save_repo_settings(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.sync.settings_root.clone() else {
            return;
        };
        let ours = self.sync.settings.clone();
        let base = self.sync.settings_base.clone();
        let file = repo_settings::settings_file(&root);
        // Merged onto the file as it is now: another Gitweft on this repository may have
        // changed other settings meanwhile.
        let job = self.jobs.spawn(Lane::Write, {
            let root = root.clone();
            move |_| repo_settings::save_merged(&root, &base, &ours).map(|(_, saved)| (ours, saved))
        });
        cx.spawn(async move |this, cx| match job.join().await {
            Some(Ok((sent, saved))) => {
                this.update(cx, |this, cx| {
                    // What this tab sent is what it knows of the file now: the next save
                    // merges its changes since onto whatever is there then.
                    if this.sync.settings_root.as_ref() == Some(&root) {
                        this.sync.settings_base = sent;
                    }
                    if let Some(notice) = data_notice(&file, &saved) {
                        crate::tab_notes::note_soon(notice, cx);
                    }
                })
                .ok();
            }
            Some(Err(err)) => {
                // Hidden branches, the graph's width, ignored blame commits: not kept.
                let text = t!("repo_settings.save_failed", error = err.to_string()).into_owned();
                this.update(cx, |_, cx| crate::tab_notes::note_failure(text, cx))
                    .ok();
            }
            None => {}
        })
        .detach();
    }

    /// The "More" menu's checkbox. Turning it on fetches right away, so a remote that
    /// can't be reached shows at once.
    pub(super) fn toggle_background_fetch(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        if self.sync.settings_root.is_none() {
            return;
        }
        let settings = &mut self.sync.settings;
        settings.background_fetch = !settings.background_fetch;
        let (enabled, minutes) = (settings.background_fetch, settings.fetch_interval_minutes);
        self.save_repo_settings(cx);
        self.sync.fetch_error = None;
        let message = if enabled {
            if self.remote_op.is_none() && self.sync.fetch_running.is_none() {
                self.start_background_fetch(cx);
            }
            tn!("background_fetch.on", count = minutes)
        } else {
            if let Some(cancel) = self.sync.fetch_running.take() {
                cancel.cancel();
            }
            t!("background_fetch.off")
        };
        note(Toast::info(message.into_owned()), cx);
        cx.notify();
    }

    /// The "More" menu's interval: minutes between background fetches for this repository.
    /// A running schedule takes it from the next fetch on.
    pub(super) fn set_fetch_interval(
        &mut self,
        minutes: u32,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.sync.settings_root.is_none() || self.sync.settings.fetch_interval_minutes == minutes
        {
            return;
        }
        self.sync.settings.fetch_interval_minutes = minutes.max(1);
        self.save_repo_settings(cx);
        let message = if self.sync.settings.background_fetch {
            tn!("background_fetch.on", count = minutes)
        } else {
            tn!("background_fetch.interval_set", count = minutes)
        };
        note(Toast::info(message.into_owned()), cx);
        cx.notify();
    }

    /// The Fetch button's tooltip, and the badge when the last background fetch failed.
    pub(super) fn fetch_tooltip(&self) -> Option<String> {
        self.sync.settings.background_fetch.then(|| {
            tn!(
                "background_fetch.tooltip",
                count = self.sync.settings.fetch_interval_minutes
            )
            .into_owned()
        })
    }

    pub(super) fn render_fetch_badge(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if let Some(error) = self.sync.fetch_error.clone() {
            let tooltip: SharedString =
                t!("background_fetch.failed_tooltip", error = error.clone()).into();
            // The kit's text tooltips don't wrap: a git error (often with a long path or URL)
            // ran on in one line over the title bar. This one is a text block with a
            // width of its own, so it wraps (a width on the tooltip's box doesn't reach the
            // text: it spilled out of the box).
            return Some(
                div()
                    .id("background-fetch-failed-tip")
                    .tooltip(move |window, cx| {
                        let text = tooltip.clone();
                        let width = cx.metrics().dp(420.);
                        gpui_kit::component::tooltip::Tooltip::element(move |_, _| {
                            div().max_w(width).whitespace_normal().child(text.clone())
                        })
                        .build(window, cx)
                    })
                    .child(
                        Button::new("background-fetch-failed")
                            .warning()
                            .xsmall()
                            .icon(Icon::from(IconName::TriangleAlert))
                            .on_click(cx.listener(move |_, _, _, cx| {
                                note(
                                    Toast::error(error.clone())
                                        .title(t!("background_fetch.failed_title").into_owned()),
                                    cx,
                                )
                            })),
                    )
                    .into_any_element(),
            );
        }
        (self.sync.fetch_running.is_some() && self.remote_op.is_none()).then(|| {
            div()
                .text_size(cx.metrics().text.xs)
                .text_color(cx.palette().fg3)
                .child(t!("background_fetch.running").into_owned())
                .into_any_element()
        })
    }

    /// The LFS banner (not installed, not set up, or files still to download) and the
    /// shallow-clone banner.
    pub(super) fn render_sync_banners(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let busy = self.remote_op.is_some();
        let mut banners = Vec::new();
        if let Some(lfs) = self
            .sync
            .extras
            .lfs
            .as_ref()
            .filter(|_| !self.sync.lfs_dismissed)
        {
            let missing = self.sync.extras.lfs_missing;
            let (message, download) = if lfs.version.is_none() {
                (t!("lfs.not_installed").into_owned(), false)
            } else if !lfs.filter_configured {
                (t!("lfs.not_set_up").into_owned(), false)
            } else if missing > 0 {
                (tn!("lfs.missing", count = missing).into_owned(), true)
            } else {
                (String::new(), false)
            };
            if !message.is_empty() {
                let mut buttons = Vec::new();
                // Installed but not set up in this clone: `git lfs install --local`.
                if lfs.version.is_some() && !lfs.filter_configured {
                    buttons.push(
                        Button::new("lfs-set-up")
                            .primary()
                            .label(t!("lfs.set_up").into_owned())
                            .disabled(busy)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.lfs_set_up(window, cx)),
                            ),
                    );
                }
                if download {
                    buttons.push(
                        Button::new("lfs-download")
                            .primary()
                            .label(t!("lfs.download").into_owned())
                            .disabled(busy)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.run_remote(RemoteOpKind::LfsPull, window, cx)
                            })),
                    );
                }
                buttons.push(
                    Button::new("lfs-dismiss")
                        .ghost()
                        .label(t!("lfs.dismiss").into_owned())
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.sync.lfs_dismissed = true;
                            cx.notify();
                        })),
                );
                banners.push(banner(message, buttons, cx));
            }
        }
        if let Some(first) = self.sync.extras.shallow.first()
            && !self.sync.shallow_dismissed
        {
            let sha = first.to_hex_with_len(8).to_string();
            // Counted on the other commits: with none, the sentence has no count at all.
            let message = match self.sync.extras.shallow.len() {
                1 => t!("shallow.banner_single", sha = sha),
                n => tn!("shallow.banner_more", count = n - 1, sha = sha),
            }
            .into_owned();
            let buttons = vec![
                Button::new("shallow-deepen")
                    .primary()
                    .label(tn!("shallow.deepen", count = DEEPEN_BY).into_owned())
                    .disabled(busy)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.run_remote(RemoteOpKind::Deepen(Deepen::By(DEEPEN_BY)), window, cx)
                    })),
                Button::new("shallow-unshallow")
                    .label(t!("shallow.unshallow").into_owned())
                    .disabled(busy)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.run_remote(RemoteOpKind::Deepen(Deepen::All), window, cx)
                    })),
                Button::new("shallow-dismiss")
                    .ghost()
                    .label(t!("shallow.dismiss").into_owned())
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.sync.shallow_dismissed = true;
                        cx.notify();
                    })),
            ];
            banners.push(banner(message, buttons, cx));
        }
        banners
    }
}

/// A one-line banner under the toolbar, with the info icon.
fn banner(message: String, buttons: Vec<Button>, cx: &App) -> AnyElement {
    let look = cx.look().clone();
    let buttons = buttons
        .into_iter()
        .map(IntoElement::into_any_element)
        .collect();
    widgets::banner(AppIcon::Info, look.palette.acc, message, buttons, &look).into_any_element()
}
