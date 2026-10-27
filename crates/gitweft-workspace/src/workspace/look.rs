//! Following the settings in a tab. Syntax colors are computed on jobs for one palette, and
//! the lines expanded with one tab width, so after a light/dark switch or a Tab width change
//! the open diff, preview, file and blame are read again (#80).

use super::*;

impl Workspace {
    /// The settings changed; the shell calls this for every tab after applying them.
    pub(crate) fn settings_changed(&mut self, cx: &mut Context<Self>) {
        // The next diff opens with the defaults (a diff changes them too, in any tab).
        let diff = &SettingsStore::settings(cx).diff;
        self.diff_layout = view_layout(diff.layout);
        self.diff_ignore_whitespace = diff.ignore_whitespace;
        let tab = diff.tab_width();
        if let Some(graph) = &self.graph_view {
            graph.update(cx, |graph, cx| graph.settings_changed(cx));
        }
        let dark = cx.palette().dark;
        if dark != self.highlight_dark || tab != self.highlight_tab {
            self.highlight_dark = dark;
            self.highlight_tab = tab;
            self.rehighlight(cx);
        }
        cx.notify();
    }

    /// Reloads the open diff in place, and reads an open file or blame's text again with
    /// the new colors. What they show stays: a blame keeps its attribution, since only its
    /// text is replaced.
    fn rehighlight(&mut self, cx: &mut Context<Self>) {
        if self.diff_view.is_some() {
            self.reload_diff(cx);
        }
        if let Some((commit, parent, change)) = self.details_view.read(cx).selected_file() {
            self.load_preview(commit, parent, change, cx);
        }
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let git = repo.git.clone();
        let cwd = repo.cwd();
        let (dark, tab) = (self.highlight_dark, self.highlight_tab);

        if let Some(view) = self.file_view.clone()
            && let Some((commit, path)) = view.read(cx).source().cloned()
        {
            let (git, cwd) = (git.clone(), cwd.clone());
            let job = self.jobs.spawn(
                Lane::Latest("file"),
                move |cancel| -> Result<FileText, String> {
                    let bytes =
                        gitweft_git::read::file_content(&git, &cwd, Some(commit), &path, cancel)
                            .map_err(|err| err.to_string())?;
                    file_text(&bytes, &path.to_string(), dark, tab)
                        .ok_or_else(|| t!("file.binary").into_owned())
                },
            );
            let view = view.downgrade();
            cx.spawn(async move |_, cx| {
                if let Some(result) = job.join().await {
                    view.update(cx, |view, cx| view.set_text(result, cx)).ok();
                }
            })
            .detach();
        }

        if let Some(view) = self.blame_view.clone()
            && let Some((path, rev)) = view.read(cx).source.clone()
        {
            // Not the blame's own lane: that would cancel a blame still streaming.
            let job = self.jobs.spawn(
                Lane::Latest("blame-text"),
                move |cancel| -> Result<FileText, String> {
                    let bytes = gitweft_git::read::file_content(&git, &cwd, rev, &path, cancel)
                        .map_err(|err| err.to_string())?;
                    file_text(&bytes, &path.to_string(), dark, tab)
                        .ok_or_else(|| t!("blame.binary").into_owned())
                },
            );
            let view = view.downgrade();
            cx.spawn(async move |_, cx| {
                if let Some(Ok(text)) = job.join().await {
                    view.update(cx, |view, cx| view.set_text(text, cx)).ok();
                }
            })
            .detach();
        }
    }
}
