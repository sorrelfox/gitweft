//! The conflict tool's header: which operation stopped on conflicts and between which
//! branches, how many files are still conflicted, and the operation's Abort / Continue.
//! It replaces the operation banner while the tool is open.

use super::*;

impl Workspace {
    pub(super) fn render_conflict_header(
        &self,
        view: &Entity<ConflictView>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let look = cx.look().clone();
        let (p, m) = (look.palette, look.metrics);
        let z = |value: f32| px(value * m.zoom);
        let (ours_label, theirs) = view.read(cx).labels().unwrap_or_default();
        // git writes HEAD for our side: name the branch instead.
        let head = match &self.state {
            RepoState::Loaded(repo) => repo.refs.head.branch_name().map(str::to_owned),
            _ => None,
        };
        let ours = match head {
            Some(branch) if ours_label == "HEAD" || ours_label.is_empty() => branch,
            _ => ours_label,
        };
        let title = match self.operation {
            Some(Operation::Merge) => {
                t!("conflict.title_merge", theirs = theirs, ours = ours)
            }
            Some(operation) => t!(
                "conflict.title_op",
                op = operation_label(operation),
                theirs = theirs,
                ours = ours
            ),
            None => t!("conflict.title_plain", ours = ours, theirs = theirs),
        }
        .into_owned();
        let highlights = name_highlights(&title, &theirs, &ours, p.lanes[3], p.lanes[2]);

        let unresolved = self
            .status
            .as_ref()
            .map_or(0, |status| status.conflicts().count());
        // The files this step stopped on, resolved ones included (the panel keeps them).
        let total = self.conflict_panel.file_count().max(unresolved);
        let subtitle = match (total, total - unresolved) {
            (count, 0) => tn!("conflict.files", count = count),
            (count, resolved) => {
                tn!(
                    "conflict.files_resolved",
                    count = count,
                    resolved = resolved
                )
            }
        }
        .into_owned();
        let busy = self.remote_op.is_some();
        let mut buttons: Vec<AnyElement> = Vec::new();
        if let Some(operation) = self.operation.filter(|op| !matches!(op, Operation::Bisect)) {
            // Outlined, as in the design; a merge's says what it aborts.
            let abort = if matches!(operation, Operation::Merge) {
                t!("conflict.abort_merge")
            } else {
                t!("op.abort")
            };
            buttons.push(
                operation_button(
                    "conflict-abort",
                    abort.into_owned(),
                    operation,
                    OperationAction::Abort,
                    false,
                    busy,
                    cx,
                )
                .outline()
                .into_any_element(),
            );
            if operation.can_continue() {
                let label = if matches!(operation, Operation::Merge) {
                    t!("conflict.commit_merge")
                } else {
                    t!("op.continue")
                };
                // Continuing needs every file resolved.
                buttons.push(
                    operation_button(
                        "conflict-continue",
                        label.into_owned(),
                        operation,
                        OperationAction::Continue,
                        true,
                        busy || unresolved > 0,
                        cx,
                    )
                    .into_any_element(),
                );
            }
        }

        h_flex()
            .min_h(z(48.))
            .flex_shrink_0()
            .px(z(14.))
            .gap(z(10.))
            .bg(p.panel)
            .border_b_1()
            .border_color(p.line)
            .child(Icon::new(AppIcon::Merge).text_color(p.modified))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        truncated("title", title)
                            .highlights(highlights)
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(p.fg),
                    )
                    .child(div().text_size(m.text.xs).text_color(p.fg3).child(subtitle)),
            )
            .children(buttons)
            .into_any_element()
    }
}

/// The branch names in a translated title, colored (theirs first, ours after it), so the
/// sentence stays one translation instead of glued fragments.
fn name_highlights(
    title: &str,
    theirs: &str,
    ours: &str,
    theirs_color: Hsla,
    ours_color: Hsla,
) -> Vec<(std::ops::Range<usize>, HighlightStyle)> {
    let style = |color: Hsla| HighlightStyle {
        color: Some(color),
        font_weight: Some(FontWeight::MEDIUM),
        ..HighlightStyle::default()
    };
    let mut ranges: Vec<(std::ops::Range<usize>, HighlightStyle)> = Vec::new();
    for (name, color) in [(theirs, theirs_color), (ours, ours_color)] {
        if name.is_empty() {
            continue;
        }
        // The first occurrence not already colored (the two names can be the same).
        let found = title
            .match_indices(name)
            .map(|(start, _)| start)
            .find(|&start| {
                ranges
                    .iter()
                    .all(|(range, _)| start + name.len() <= range.start || start >= range.end)
            });
        if let Some(start) = found {
            ranges.push((start..start + name.len(), style(color)));
        }
    }
    ranges.sort_by_key(|(range, _)| range.start);
    ranges
}

#[cfg(test)]
mod tests {
    use super::name_highlights;
    use gpui_kit::{Hsla, black, white};

    #[test]
    fn names_are_found_once_each_in_the_title() {
        let (a, b): (Hsla, Hsla) = (white(), black());
        let ranges: Vec<_> = name_highlights("Merging topic into main", "topic", "main", a, b)
            .into_iter()
            .map(|(range, _)| range)
            .collect();
        assert_eq!(ranges, [8..13, 19..23]);
        // The same name on both sides: two different places.
        let same: Vec<_> = name_highlights("Merging main into main", "main", "main", a, b)
            .into_iter()
            .map(|(range, _)| range)
            .collect();
        assert_eq!(same, [8..12, 18..22]);
    }
}
