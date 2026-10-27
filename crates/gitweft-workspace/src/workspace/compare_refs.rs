//! Comparing any two refs or commits: the palette's choice of what to compare with, and
//! swapping the sides of an open comparison.

use super::commands::command;
use super::*;

impl Workspace {
    /// Lists in the palette what `head` (a ref or commit, and its name shown) can be
    /// compared with: the checked-out branch first, then the other local branches, the
    /// remote branches and the tags.
    pub(super) fn choose_compare_base(
        &mut self,
        head: String,
        head_name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let this = cx.entity().downgrade();
        let group: SharedString = t!("compare.with_group", name = head_name)
            .into_owned()
            .into();
        let refs = &repo.refs;
        let current = refs
            .head
            .branch_name()
            .map(|name| format!("refs/heads/{name}"));
        let mut commands = Vec::new();
        if current.as_deref() != Some(head.as_str()) {
            let (head, head_name) = (head.clone(), head_name.clone());
            let label = refs.head.branch_name().unwrap_or("HEAD").to_owned();
            commands.push(
                command(&this, &group, label, move |w, window, cx| {
                    w.open_compare_refs(None, head.clone(), head_name.clone(), window, cx)
                })
                .detail(t!("compare.current")),
            );
        }
        let kinds = [
            (&refs.local_branches, t!("compare.kind_branch")),
            (&refs.remote_branches, t!("compare.kind_remote")),
            (&refs.tags, t!("compare.kind_tag")),
        ];
        for (entries, kind) in kinds {
            for entry in entries {
                let full_name = entry.full_name.to_string();
                if full_name == head || Some(&full_name) == current.as_ref() {
                    continue;
                }
                let base = (full_name, entry.short_name.clone());
                let (head, head_name) = (head.clone(), head_name.clone());
                commands.push(
                    command(
                        &this,
                        &group,
                        entry.short_name.clone(),
                        move |w, window, cx| {
                            let base = Some(base.clone());
                            w.open_compare_refs(base, head.clone(), head_name.clone(), window, cx)
                        },
                    )
                    .detail(kind.clone()),
                );
            }
        }
        crate::palette::open(commands, window, cx);
    }

    /// The open comparison with its sides swapped: what was compared becomes the base.
    pub(super) fn swap_compare(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (RepoState::Loaded(repo), Some(view), Some(head)) =
            (&self.state, &self.compare_view, self.compare_head.clone())
        else {
            return;
        };
        let (base_name, head_name) = view.read(cx).names();
        let base = match self.compare_base.clone() {
            Some((rev, _)) => rev,
            None => match repo.refs.head.branch_name() {
                Some(name) => format!("refs/heads/{name}"),
                None => "HEAD".to_owned(),
            },
        };
        self.open_compare_refs(Some((head, head_name)), base, base_name, window, cx);
    }
}
