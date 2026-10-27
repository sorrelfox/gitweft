//! The status bar under a repository: how the branch stands against its upstream and when
//! the remotes were last fetched, the number of commits, a running network operation with
//! Cancel, and the branch with its counts on the right. Also the branch summary shown in
//! the title bar.

use std::time::UNIX_EPOCH;

use gitweft_ui::widgets;

use super::*;

impl Workspace {
    /// For the title bar: what HEAD is ("main", "detached at …") and how far the branch is
    /// from its upstream ("2 ahead"), or `None` when no repository is open.
    pub fn title_branch(&self) -> Option<(String, Option<String>)> {
        let RepoState::Loaded(repo) = &self.state else {
            return None;
        };
        let detail = self
            .status
            .as_ref()
            .and_then(|status| counts_text(status.branch.ahead, status.branch.behind));
        Some((head_label(&repo.refs.head), detail))
    }

    /// Which lane color the checked-out commit has in the graph (the first one until the
    /// graph is loaded), for the branch icon in the title bar and the sidebar's marker.
    pub fn head_lane(&self, cx: &App) -> usize {
        self.graph_view
            .as_ref()
            .and_then(|graph| graph.read(cx).head_color())
            .unwrap_or(0)
    }

    pub(super) fn render_status_bar(
        &self,
        repo: &LoadedRepo,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let look = cx.look().clone();
        let (p, m) = (look.palette, look.metrics);
        let branch = self.status.as_ref().map(|status| &status.branch);
        let upstream = branch.and_then(|branch| {
            branch
                .upstream
                .as_deref()
                .map(|upstream| (upstream, branch.ahead, branch.behind))
        });
        let detached = matches!(repo.refs.head, HeadInfo::Detached { .. });
        let (tint, state) = match upstream {
            _ if detached => (p.fg3, t!("status_bar.detached").into_owned()),
            None => (p.fg3, t!("status_bar.no_upstream").into_owned()),
            Some((upstream, 0, 0)) => {
                // "Up to date with origin", as in the design: the remote is enough.
                let remote = upstream.split('/').next().unwrap_or(upstream);
                (
                    p.add,
                    t!("status_bar.up_to_date", upstream = remote).into_owned(),
                )
            }
            Some((upstream, ahead, 0)) => (
                p.acc,
                tn!("status_bar.ahead", count = ahead, upstream = upstream).into_owned(),
            ),
            Some((upstream, 0, behind)) => (
                p.modified,
                tn!("status_bar.behind", count = behind, upstream = upstream).into_owned(),
            ),
            Some((upstream, ahead, behind)) => (
                p.modified,
                t!(
                    "status_bar.diverged",
                    ahead = tn!("status_bar.ahead", count = ahead, upstream = upstream),
                    behind = tn!("status_bar.diverged_behind", count = behind)
                )
                .into_owned(),
            ),
        };
        let fetched = self
            .sync
            .extras
            .fetched_at
            .and_then(|at| at.duration_since(UNIX_EPOCH).ok())
            .map(|since| {
                t!(
                    "status_bar.fetched",
                    when = crate::util::ago(since.as_secs() as i64)
                )
                .into_owned()
            });
        let left = match fetched {
            Some(fetched) => t!("common.middle_dot", first = state, second = fetched).into_owned(),
            None => state,
        };
        let rows = repo.graph.rows();
        let commits = tn!("status_bar.commits", count = rows => thousands(rows)).into_owned();
        // The branch and, as in the sidebar, only the counts that aren't zero.
        let mut right = head_label(&repo.refs.head);
        if let Some((_, ahead, behind)) = upstream {
            for (count, arrow) in [(ahead, '↑'), (behind, '↓')] {
                if count > 0 {
                    right.push_str(&format!(" {arrow}{count}"));
                }
            }
        }

        h_flex()
            .h(m.status_bar)
            .flex_shrink_0()
            .px(m.dp(12.))
            .gap(m.dp(16.))
            .bg(p.panel)
            .border_t_1()
            .border_color(p.line)
            .text_size(m.text.xs)
            .text_color(p.fg3)
            .child(
                h_flex()
                    .gap(m.dp(6.))
                    .min_w_0()
                    .child(widgets::dot(tint, m.dp(6.)))
                    .child(truncated("status-left", left)),
            )
            .child(div().flex_shrink_0().child(commits))
            .children(self.render_running_operation(&p, cx))
            .children(self.render_waiting_writes(&p))
            .child(div().flex_1())
            .child(
                div()
                    .flex_shrink_0()
                    .font_family(gitweft_ui::current::mono_font())
                    .child(right),
            )
    }

    /// Changes (a stage, a commit…) queued behind a long job of the write lane: a bisect run,
    /// a fetch, pull or push, the background fetch. They run in order once it's done; until
    /// then the status bar says they wait, so nothing seems to have been ignored.
    fn render_waiting_writes(&self, p: &Palette) -> Option<Div> {
        let long = self.bisect_running.is_some()
            || self.remote_op.is_some()
            || self.sync.fetch_running.is_some();
        // The long job itself is one of them. No count: the lane also carries the app's own
        // bookkeeping (the undo log, settings), which the user never asked for.
        let waiting = self.jobs.pending_writes() > 1;
        (long && waiting).then(|| {
            div()
                .flex_shrink_0()
                .text_color(p.modified)
                .child(t!("status_bar.writes_waiting").into_owned())
        })
    }

    /// A fetch, pull or push that is running: what it is, its progress, and Cancel.
    fn render_running_operation(&self, p: &Palette, cx: &mut Context<Self>) -> Option<Div> {
        let op = self.remote_op.as_ref()?;
        let m = *cx.metrics();
        let detail = op.progress.as_ref().map(|progress| match progress.percent {
            Some(percent) => {
                t!("remote.progress", phase = progress.phase, percent = percent).into_owned()
            }
            None => progress.phase.clone(),
        });
        // Cancel comes before the progress, whose text changes length all the time
        // (percentages, a submodule's folder), so the button stays where it is.
        Some(
            h_flex()
                .min_w_0()
                .gap(m.dp(8.))
                .text_color(p.fg2)
                .child(widgets::dot(p.acc, m.dp(6.)))
                .child(div().flex_shrink_0().child(op.kind.running_label()))
                .child(
                    Button::new("cancel-remote")
                        .ghost()
                        .xsmall()
                        .label(t!("toolbar.cancel").into_owned())
                        .on_click(cx.listener(|this, _, _, cx| this.cancel_remote(cx))),
                )
                .children(detail.map(|detail| truncated("remote-detail", detail))),
        )
    }
}

/// "2 ahead", "3 behind", "2 ahead, 3 behind"; `None` when level with the upstream.
fn counts_text(ahead: u32, behind: u32) -> Option<String> {
    match (ahead, behind) {
        (0, 0) => None,
        (ahead, 0) => Some(tn!("title.ahead", count = ahead).into_owned()),
        (0, behind) => Some(tn!("title.behind", count = behind).into_owned()),
        (ahead, behind) => Some(
            t!(
                "title.ahead_behind",
                ahead = tn!("title.ahead", count = ahead),
                behind = tn!("title.behind", count = behind)
            )
            .into_owned(),
        ),
    }
}

/// `1247` → `1,247`, with the language's separator (`number.thousands_separator`).
fn thousands(n: usize) -> String {
    grouped(n, &t!("number.thousands_separator"))
}

/// `n` with `separator` between groups of three digits.
fn grouped(n: usize, separator: &str) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 * separator.len());
    for (ix, digit) in digits.chars().enumerate() {
        if ix > 0 && (digits.len() - ix).is_multiple_of(3) {
            out.push_str(separator);
        }
        out.push(digit);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{counts_text, grouped, thousands};

    #[test]
    fn two_counts_each_take_their_form() {
        assert_eq!(counts_text(0, 0), None);
        assert_eq!(counts_text(1, 0).as_deref(), Some("1 ahead"));
        assert_eq!(counts_text(1, 2).as_deref(), Some("1 ahead, 2 behind"));
    }

    #[test]
    fn thousands_separators() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_247), "1,247");
        assert_eq!(thousands(74_123), "74,123");
        assert_eq!(thousands(1_000_000), "1,000,000");
        // Other languages' separators, from their locale file.
        assert_eq!(grouped(1_247, "."), "1.247");
        assert_eq!(grouped(1_000_000, "\u{202f}"), "1\u{202f}000\u{202f}000");
        assert_eq!(grouped(999, "."), "999");
    }
}
