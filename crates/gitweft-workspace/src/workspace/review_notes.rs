//! A review saved as a git note on the compared head (`refs/notes/review`), so it travels
//! with the repository (pushed like any ref, if the user wants).

use super::*;

/// Where review notes go (`refs/notes/review`).
const NOTES_REF: &str = "review";

impl Workspace {
    /// Writes the review's Markdown as the note of `head` (replacing an older one), in the
    /// write lane, and says where it went.
    pub(super) fn note_review(
        &mut self,
        markdown: String,
        head: ObjectId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let git = repo.git.clone();
        let cwd = repo.cwd();
        let job = self.jobs.spawn(Lane::Write, move |cancel| {
            gitweft_git::notes::add_note(&git, &cwd, NOTES_REF, head, &markdown, cancel)
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = job.join().await;
            this.update_in(cx, |_, _, cx| match result {
                Some(Ok(())) => {
                    let sha = head.to_hex_with_len(8).to_string();
                    let message = t!("review.noted", sha = sha).into_owned();
                    note(Toast::success(message), cx);
                }
                Some(Err(err)) => note(
                    Toast::error(err.summary()).title(t!("review.note_failed").into_owned()),
                    cx,
                ),
                None => {}
            })
            .ok();
        })
        .detach();
    }
}
