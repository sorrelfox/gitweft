//! Local code review sessions, one per reviewed branch: what it's reviewed against,
//! which files were viewed (and at which content), comments, and the range reviewed
//! last. Kept as JSON in `<repo data dir>/reviews/`; nothing leaves the machine.
//! Callers run these in a job: they do file I/O.

use std::io;
use std::path::{Path, PathBuf};

use gitweft_git::read::{
    FileChange, RangePair, changed_files, count_commits, is_ancestor, range_diff,
};
use gitweft_git::{CancelToken, GitCli, ObjectId};
use serde::{Deserialize, Serialize};

use crate::paths::{file_name_part, fnv1a, repo_data_dir};

const VERSION: u32 = 1;

/// How many deleted comments a review keeps for restoring.
pub const DELETED_KEPT: usize = 20;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSession {
    version: u32,
    /// The branch under review (full ref name, e.g. `refs/heads/topic`).
    pub branch: String,
    /// What it's reviewed against (full ref name, e.g. `refs/heads/main`).
    pub base: String,
    /// The range as it was when the review was last marked done: "since last review"
    /// and the range-diff after a rebase start from here.
    #[serde(default)]
    pub reviewed: Option<ReviewedRange>,
    /// Files marked viewed, each at the content (blob) it had then.
    #[serde(default)]
    pub viewed: Vec<ViewedFile>,
    #[serde(default)]
    pub comments: Vec<Comment>,
    /// Comments deleted lately, the newest first (at most [`DELETED_KEPT`]): one can be restored.
    #[serde(default)]
    pub deleted: Vec<Comment>,
    /// The id the next comment gets.
    #[serde(default)]
    next_comment: u64,
}

/// A reviewed range: the merge base and the branch head.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewedRange {
    #[serde(with = "hex_id")]
    pub base: ObjectId,
    #[serde(with = "hex_id")]
    pub head: ObjectId,
}

/// A file's path as a review keeps it (in [`ViewedFile`] and [`Comment`]): itself when it's
/// UTF-8, else encoded so the saved JSON holds its bytes (a NUL and hex, as the undo log
/// keeps names). Two names that only show alike stay two files.
pub fn path_key(path: &[u8]) -> String {
    gitweft_git::branch::config_value_text(path)
}

/// The path a [`path_key`] names, byte for byte, for git.
pub fn path_bytes(key: &str) -> Vec<u8> {
    gitweft_git::branch::config_value_bytes(key)
}

/// A [`path_key`]'s path as text to show (lossy for one that isn't UTF-8).
pub fn path_shown(key: &str) -> String {
    String::from_utf8_lossy(&path_bytes(key)).into_owned()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewedFile {
    /// A [`path_key`].
    pub path: String,
    #[serde(with = "hex_id")]
    pub blob: ObjectId,
}

/// Which side of a diff a comment is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Old,
    New,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Comment {
    pub id: u64,
    /// A [`path_key`].
    pub path: String,
    pub side: Side,
    /// The 1-based line on that side when the comment was written (or last re-anchored).
    pub line: u32,
    /// The line and its surroundings, to find it again after the branch changes.
    pub anchor: Anchor,
    pub body: String,
    /// Who wrote it: git's `user.name` then; empty for comments saved before.
    #[serde(default)]
    pub author: String,
    /// The branch head the comment was written against.
    #[serde(with = "hex_id")]
    pub commit: ObjectId,
    /// Seconds since the Unix epoch.
    pub created: u64,
    #[serde(default)]
    pub resolved: bool,
    /// Its line couldn't be found in the current version.
    #[serde(default)]
    pub outdated: bool,
}

/// A commented line's text, with up to a few lines of context on each side.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Anchor {
    pub text: String,
    #[serde(default)]
    pub before: Vec<String>,
    #[serde(default)]
    pub after: Vec<String>,
}

/// How many neighbouring lines an anchor keeps on each side.
pub const ANCHOR_CONTEXT: usize = 2;

impl Anchor {
    /// The anchor of `lines[index]`, with up to [`ANCHOR_CONTEXT`] neighbours each way.
    pub fn at(lines: &[&str], index: usize) -> Self {
        let start = index.saturating_sub(ANCHOR_CONTEXT);
        let end = (index + 1 + ANCHOR_CONTEXT).min(lines.len());
        let owned = |range: &[&str]| range.iter().map(|line| (*line).to_owned()).collect();
        Self {
            text: lines.get(index).copied().unwrap_or_default().to_owned(),
            before: owned(&lines[start..index.min(lines.len())]),
            after: owned(lines.get(index + 1..end).unwrap_or_default()),
        }
    }

    /// Where the anchored line is in `lines` now (1-based), given that it was at `line`;
    /// `None` when it's gone. Among lines with the same text, the one whose neighbours
    /// match best wins, then the one nearest the old place. A blank line needs at least
    /// one matching neighbour to be recognised. Trailing whitespace doesn't count.
    pub fn find(&self, lines: &[&str], line: u32) -> Option<u32> {
        let same = |a: &str, b: &str| a.trim_end() == b.trim_end();
        let was = (line as usize).saturating_sub(1);
        let score = |index: usize| {
            let before = self
                .before
                .iter()
                .rev()
                .enumerate()
                .take_while(|(k, text)| index > *k && same(lines[index - 1 - k], text))
                .count();
            let after = self
                .after
                .iter()
                .enumerate()
                .take_while(|(k, text)| {
                    lines
                        .get(index + 1 + k)
                        .is_some_and(|line| same(line, text))
                })
                .count();
            before + after
        };
        let blank = self.text.trim().is_empty();
        lines
            .iter()
            .enumerate()
            .filter(|(_, text)| same(text, &self.text))
            .map(|(index, _)| (index, score(index)))
            .filter(|&(_, score)| !blank || score > 0)
            .max_by(|(a, score_a), (b, score_b)| {
                score_a
                    .cmp(score_b)
                    .then_with(|| was.abs_diff(*b).cmp(&was.abs_diff(*a)))
            })
            .map(|(index, _)| index as u32 + 1)
    }
}

/// What a new comment is about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewComment {
    pub path: String,
    pub side: Side,
    pub line: u32,
    pub anchor: Anchor,
    pub body: String,
    /// git's `user.name` (empty when it isn't set).
    pub author: String,
    pub commit: ObjectId,
    pub created: u64,
}

impl ReviewSession {
    pub fn new(branch: String, base: String) -> Self {
        Self {
            version: VERSION,
            branch,
            base,
            reviewed: None,
            viewed: Vec::new(),
            comments: Vec::new(),
            deleted: Vec::new(),
            next_comment: 1,
        }
    }

    /// Whether `path` was marked viewed at exactly this content: a change to the file
    /// clears the mark by itself.
    pub fn is_viewed(&self, path: &str, blob: ObjectId) -> bool {
        self.viewed
            .iter()
            .any(|file| file.path == path && file.blob == blob)
    }

    /// Marks `path` viewed at `blob` (replacing an older mark), or clears its mark.
    pub fn set_viewed(&mut self, path: &str, blob: ObjectId, viewed: bool) {
        self.viewed.retain(|file| file.path != path);
        if viewed {
            self.viewed.push(ViewedFile {
                path: path.to_owned(),
                blob,
            });
        }
    }

    /// Carries viewed marks over renames: a file of `files` (the files the compared
    /// ref has, each with its current blob; not the deleted ones) without a mark of its own
    /// takes a mark with the same content from a path that isn't among `files` any more, so
    /// a `git mv` keeps it viewed. A file renamed and changed has no such mark: it's
    /// unviewed, as any changed file. Returns whether a mark moved.
    pub fn carry_marks(&mut self, files: &[(String, ObjectId)]) -> bool {
        let mut changed = false;
        for (path, blob) in files {
            if self.viewed.iter().any(|mark| &mark.path == path) {
                continue;
            }
            let gone = |mark: &ViewedFile| !files.iter().any(|(path, _)| *path == mark.path);
            if let Some(mark) = self
                .viewed
                .iter_mut()
                .find(|mark| mark.blob == *blob && gone(mark))
            {
                mark.path = path.clone();
                changed = true;
            }
        }
        changed
    }

    /// Records `base..head` as the range reviewed last.
    pub fn mark_reviewed(&mut self, base: ObjectId, head: ObjectId) {
        self.reviewed = Some(ReviewedRange { base, head });
    }

    /// Adds a comment and returns its id: above every id so far, and made from the clock and
    /// this process, so two Gitweft instances adding comments to one review pick different
    /// ones (their saves merge by id: [`ReviewSession::merged`]).
    pub fn add_comment(&mut self, comment: NewComment) -> u64 {
        let id = self.next_comment.max(fresh_comment_id()).max(1);
        self.next_comment = id + 1;
        self.comments.push(Comment {
            id,
            path: comment.path,
            side: comment.side,
            line: comment.line,
            anchor: comment.anchor,
            body: comment.body,
            author: comment.author,
            commit: comment.commit,
            created: comment.created,
            resolved: false,
            outdated: false,
        });
        id
    }

    pub fn comment_mut(&mut self, id: u64) -> Option<&mut Comment> {
        self.comments.iter_mut().find(|comment| comment.id == id)
    }

    /// Deletes a comment: it goes to [`Self::deleted`], from where it can be restored.
    pub fn remove_comment(&mut self, id: u64) {
        if let Some(ix) = self.comments.iter().position(|comment| comment.id == id) {
            let comment = self.comments.remove(ix);
            self.deleted.insert(0, comment);
            self.deleted.truncate(DELETED_KEPT);
        }
    }

    /// Brings a deleted comment back, as it was (same id, place and text); returns whether
    /// it was among the deleted ones.
    pub fn restore_comment(&mut self, id: u64) -> bool {
        let Some(ix) = self.deleted.iter().position(|comment| comment.id == id) else {
            return false;
        };
        let comment = self.deleted.remove(ix);
        self.comments.push(comment);
        self.comments.sort_by_key(|comment| comment.id);
        true
    }

    /// The files (and sides) that have comments, each once.
    pub fn commented(&self) -> Vec<(String, Side)> {
        let mut commented: Vec<(String, Side)> = self
            .comments
            .iter()
            .map(|comment| (comment.path.clone(), comment.side))
            .collect();
        commented.sort();
        commented.dedup();
        commented
    }

    /// Places the comments on `side` of `path` in its current text: each moves to where
    /// its line is now, or is marked outdated (and comes back if the line returns).
    /// Returns whether anything changed.
    pub fn reanchor(&mut self, path: &str, side: Side, lines: &[&str]) -> bool {
        let mut changed = false;
        for comment in self
            .comments
            .iter_mut()
            .filter(|comment| comment.path == path && comment.side == side)
        {
            let (line, outdated) = match comment.anchor.find(lines, comment.line) {
                Some(line) => (line, false),
                None => (comment.line, true),
            };
            if (line, outdated) != (comment.line, comment.outdated) {
                comment.line = line;
                comment.outdated = outdated;
                changed = true;
            }
        }
        changed
    }

    /// This instance's changes since `base` (the session it last read or saved) applied onto
    /// `theirs` (the file as another Gitweft may have left it), so neither undoes the other:
    /// comments by id (added, changed or removed here since `base` wins, theirs
    /// otherwise; one changed on one side and removed on the other stays; a comment added
    /// here under an id another one took gets a new id), viewed marks by file (the files
    /// whose mark changed here take ours), the reviewed range if it changed here.
    pub fn merged(base: &Self, ours: &Self, theirs: &Self) -> Self {
        fn find(list: &[Comment], id: u64) -> Option<&Comment> {
            list.iter().find(|comment| comment.id == id)
        }
        let mut comments = Vec::new();
        for there in &theirs.comments {
            match (
                find(&base.comments, there.id),
                find(&ours.comments, there.id),
            ) {
                (Some(was), Some(mine)) if mine != was => comments.push(mine.clone()),
                // Removed here, and not changed there since: gone.
                (Some(was), None) if there == was => {}
                _ => comments.push(there.clone()),
            }
        }
        let highest = ours
            .comments
            .iter()
            .chain(&theirs.comments)
            .map(|comment| comment.id)
            .max()
            .unwrap_or(0);
        let mut next = ours.next_comment.max(theirs.next_comment).max(highest + 1);
        for mine in &ours.comments {
            match (
                find(&base.comments, mine.id),
                find(&theirs.comments, mine.id),
            ) {
                // Changed here, removed there: kept.
                (Some(was), None) if mine != was => comments.push(mine.clone()),
                // Added here.
                (None, None) => comments.push(mine.clone()),
                (None, Some(there)) if there != mine => {
                    let mut renumbered = mine.clone();
                    renumbered.id = next;
                    next += 1;
                    comments.push(renumbered);
                }
                _ => {}
            }
        }
        comments.sort_by_key(|comment| comment.id);

        let marks = |list: &[ViewedFile], path: &str| -> Vec<ViewedFile> {
            list.iter()
                .filter(|mark| mark.path == path)
                .cloned()
                .collect()
        };
        let touched: Vec<&str> = base
            .viewed
            .iter()
            .chain(&ours.viewed)
            .map(|mark| mark.path.as_str())
            .filter(|path| marks(&base.viewed, path) != marks(&ours.viewed, path))
            .collect();
        let mut viewed: Vec<ViewedFile> = theirs
            .viewed
            .iter()
            .filter(|mark| !touched.contains(&mark.path.as_str()))
            .cloned()
            .collect();
        viewed.extend(
            ours.viewed
                .iter()
                .filter(|mark| touched.contains(&mark.path.as_str()))
                .cloned(),
        );

        // Deleted lately: ours first, then theirs, each once, and none that is a comment
        // again (restored on one side).
        let mut deleted: Vec<Comment> = Vec::new();
        for comment in ours.deleted.iter().chain(&theirs.deleted) {
            let known = |list: &[Comment]| list.iter().any(|other| other.id == comment.id);
            if !known(&comments) && !known(&deleted) {
                deleted.push(comment.clone());
            }
        }
        deleted.truncate(DELETED_KEPT);

        Self {
            version: VERSION,
            branch: ours.branch.clone(),
            base: ours.base.clone(),
            reviewed: if ours.reviewed != base.reviewed {
                ours.reviewed
            } else {
                theirs.reviewed
            },
            viewed,
            comments,
            deleted,
            next_comment: next,
        }
    }

    /// Takes the lines and outdated flags that `other` (a re-anchored copy) found for
    /// the comments both have; everything else stays. Returns whether anything changed.
    pub fn take_positions(&mut self, other: &ReviewSession) -> bool {
        let mut changed = false;
        for comment in &mut self.comments {
            if let Some(found) = other.comments.iter().find(|found| found.id == comment.id)
                && (found.line, found.outdated) != (comment.line, comment.outdated)
            {
                comment.line = found.line;
                comment.outdated = found.outdated;
                changed = true;
            }
        }
        changed
    }

    /// The review as Markdown: a title naming the branch and base, the range, then the
    /// comments grouped by file (files sorted, comments by line), each with its place,
    /// its line quoted, and its text. Resolved and outdated comments are included and marked.
    pub fn to_markdown(&self, info: &ExportInfo) -> String {
        self.to_markdown_in(info, &ExportWords::default())
    }

    /// [`Self::to_markdown`] with the export's words in another language.
    pub fn to_markdown_in(&self, info: &ExportInfo, words: &ExportWords) -> String {
        let title = fill(&words.title, &[("branch", short_ref(&self.branch))]);
        let mut blocks = vec![format!("# {title}"), self.markdown_range(info, words)];

        let mut comments: Vec<&Comment> = self.comments.iter().collect();
        comments
            .sort_by(|a, b| (&a.path, a.line, a.side, a.id).cmp(&(&b.path, b.line, b.side, b.id)));
        if comments.is_empty() {
            blocks.push(words.no_comments.clone());
        }
        let mut path: Option<&str> = None;
        for comment in comments {
            if path != Some(comment.path.as_str()) {
                path = Some(&comment.path);
                blocks.push(format!("## {}", markdown_path(&path_shown(&comment.path))));
            }
            blocks.push(markdown_place(comment, words));
            blocks.push(markdown_quote(&comment.anchor.text));
            let body = comment.body.trim_end_matches(['\n', '\r']);
            if !body.trim().is_empty() {
                blocks.push(body.to_owned());
            }
        }

        let mut text = blocks.join("\n\n");
        text.push('\n');
        text
    }

    /// "Against `main` · branch at `…` · merge base `…` · last marked reviewed at `…`".
    fn markdown_range(&self, info: &ExportInfo, words: &ExportWords) -> String {
        let base = code_span(short_ref(&self.base));
        let head = code_span(&short_id(info.head));
        let mut parts = vec![
            fill(&words.against, &[("base", &base)]),
            fill(&words.branch_at, &[("sha", &head)]),
        ];
        if let Some(merge_base) = info.merge_base {
            let sha = code_span(&short_id(merge_base));
            parts.push(fill(&words.merge_base, &[("sha", &sha)]));
        }
        if let Some(reviewed) = &self.reviewed {
            let sha = code_span(&short_id(reviewed.head));
            parts.push(fill(&words.reviewed_at, &[("sha", &sha)]));
        }
        parts.join(SEPARATOR)
    }
}

/// What an export is about, beyond the session: the heads it was written against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExportInfo {
    /// The compared branch's head and the merge base, as shown in the comparison.
    pub head: ObjectId,
    pub merge_base: Option<ObjectId>,
}

/// The words of a Markdown export: whole phrases with `%{name}` places, as in the app's
/// `locales/app.yml` (this crate has no translations; the app passes its own). `Default`
/// is English.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportWords {
    /// The heading, with `%{branch}`.
    pub title: String,
    /// The compared base, with `%{base}` (in inline code).
    pub against: String,
    /// The branch's head, with `%{sha}`.
    pub branch_at: String,
    /// The merge base, with `%{sha}`.
    pub merge_base: String,
    /// The head last marked reviewed, with `%{sha}`.
    pub reviewed_at: String,
    /// A comment's heading, with `%{line}` and `%{side}` (`old` or `new` below).
    pub place: String,
    pub old: String,
    pub new: String,
    pub resolved: String,
    pub outdated: String,
    pub no_comments: String,
    /// Who wrote a comment, with `%{author}`.
    pub by: String,
}

impl Default for ExportWords {
    fn default() -> Self {
        Self {
            title: "Review of %{branch}".into(),
            against: "Against %{base}".into(),
            branch_at: "branch at %{sha}".into(),
            merge_base: "merge base %{sha}".into(),
            reviewed_at: "last marked reviewed at %{sha}".into(),
            place: "Line %{line} (%{side})".into(),
            old: "old".into(),
            new: "new".into(),
            resolved: "resolved".into(),
            outdated: "outdated".into(),
            no_comments: "No comments.".into(),
            by: "by %{author}".into(),
        }
    }
}

/// Between the parts of the range line and a comment heading's marks.
const SEPARATOR: &str = " · ";

/// `template` with each `%{name}` replaced by its value.
fn fill(template: &str, values: &[(&str, &str)]) -> String {
    values
        .iter()
        .fold(template.to_owned(), |text, (name, value)| {
            text.replace(&format!("%{{{name}}}"), value)
        })
}

/// A branch or base name without `refs/heads/` or `refs/remotes/`.
fn short_ref(name: &str) -> &str {
    name.strip_prefix("refs/heads/")
        .or_else(|| name.strip_prefix("refs/remotes/"))
        .unwrap_or(name)
}

/// The first 8 hex characters of `id`.
fn short_id(id: ObjectId) -> String {
    let mut hex = id.to_string();
    hex.truncate(8);
    hex
}

/// `text` as inline code, with a fence longer than any run of backticks inside it.
fn code_span(text: &str) -> String {
    let longest = text
        .split(|c| c != '`')
        .map(str::len)
        .max()
        .unwrap_or_default();
    let fence = "`".repeat(longest + 1);
    let pad = if text.starts_with('`') || text.ends_with('`') {
        " "
    } else {
        ""
    };
    format!("{fence}{pad}{text}{pad}{fence}")
}

/// A path for a heading: in inline code when it has characters Markdown would act on.
fn markdown_path(path: &str) -> String {
    if path.contains(['*', '_', '`', '#', '[']) {
        code_span(path)
    } else {
        path.to_owned()
    }
}

/// "### Line 12 (new)", with " · resolved" and " · outdated" when they apply.
fn markdown_place(comment: &Comment, words: &ExportWords) -> String {
    let side = match comment.side {
        Side::Old => &words.old,
        Side::New => &words.new,
    };
    let line = comment.line.to_string();
    let place = fill(&words.place, &[("line", &line), ("side", side)]);
    let mut place = format!("### {place}");
    if !comment.author.is_empty() {
        place.push_str(SEPARATOR);
        place.push_str(&fill(&words.by, &[("author", &comment.author)]));
    }
    for (flag, word) in [
        (comment.resolved, &words.resolved),
        (comment.outdated, &words.outdated),
    ] {
        if flag {
            place.push_str(SEPARATOR);
            place.push_str(word);
        }
    }
    place
}

/// The commented line as a block quote; a blank line gives a bare `> `.
fn markdown_quote(text: &str) -> String {
    let text = text.trim_end();
    if text.is_empty() {
        return "> ".to_owned();
    }
    text.lines()
        .map(|line| format!("> {}", line.trim_end()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// What happened to the branch since the range reviewed last.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SinceReview {
    /// The branch is where it was when reviewed.
    Unchanged,
    /// New commits on top of the reviewed head: the files they changed, each with its
    /// blob in `head`.
    Advanced {
        from: ObjectId,
        commits: usize,
        files: Vec<FileChange>,
        blobs: Vec<ObjectId>,
    },
    /// The reviewed head is no longer in the branch (rebased, amended): how its commits
    /// changed.
    Rewritten { pairs: Vec<RangePair> },
}

/// Compares the branch as it is now (`base`, its current merge base, up to `head`) with
/// `reviewed`; a rewritten branch's range-diff uses `creation_factor` (`None`: git's).
/// Runs git: call it in a job.
pub fn since_review(
    git: &GitCli,
    cwd: &Path,
    reviewed: &ReviewedRange,
    base: ObjectId,
    head: ObjectId,
    creation_factor: Option<u32>,
    cancel: &CancelToken,
) -> Result<SinceReview, String> {
    if head == reviewed.head {
        return Ok(SinceReview::Unchanged);
    }
    if is_ancestor(git, cwd, &reviewed.head, &head, cancel).map_err(|err| err.to_string())? {
        let (files, blobs) = changed_files(git, cwd, &reviewed.head, &head, cancel)
            .map_err(|err| err.to_string())?;
        let commits = count_commits(git, cwd, &reviewed.head, &head, cancel)
            .map_err(|err| err.to_string())?;
        return Ok(SinceReview::Advanced {
            from: reviewed.head,
            commits,
            files,
            blobs,
        });
    }
    let pairs = range_diff(
        git,
        cwd,
        (&reviewed.base, &reviewed.head),
        (&base, &head),
        creation_factor,
        cancel,
    )
    .map_err(|err| err.to_string())?;
    Ok(SinceReview::Rewritten { pairs })
}

/// The session file of `branch` (a full ref name) in the repository at `root`.
pub fn review_file(root: &Path, branch: &str) -> PathBuf {
    let short = branch
        .strip_prefix("refs/heads/")
        .or_else(|| branch.strip_prefix("refs/remotes/"))
        .unwrap_or(branch);
    // The hash tells `a/b` and `a_b` apart; the name keeps the file recognisable.
    let name = format!(
        "{}-{:016x}.json",
        file_name_part(short),
        fnv1a(branch.as_bytes())
    );
    repo_data_dir(root).join("reviews").join(name)
}

/// The session in `file`; `None` if there is none (or it can't be read).
pub fn load(file: &Path) -> Option<ReviewSession> {
    let text = std::fs::read_to_string(file).ok()?;
    let session: ReviewSession = serde_json::from_str(&text).ok()?;
    (session.version == VERSION).then_some(session)
}

/// Writes the session through [`crate::datafile::write_json`]: a file [`load`] couldn't
/// read is kept as a backup first, and one written by a newer version is left alone, so a
/// fresh session made in its place never replaces its comments.
pub fn save(file: &Path, session: &ReviewSession) -> io::Result<crate::datafile::Saved> {
    let text = serde_json::to_string_pretty(session).map_err(io::Error::other)?;
    crate::datafile::write_json(file, u64::from(VERSION), &text)
}

/// Saves `ours` without undoing another instance's changes: holding a lock
/// (`<file>.lock`), the session as it is now in `file` is read, and
/// [`ReviewSession::merged`] with `base` (what this instance last read or saved) is
/// written. Returns what was written. A file that can't be read (or isn't there) counts as
/// nothing saved yet: `ours` is written (an unreadable file is kept aside first).
pub fn save_merged(
    file: &Path,
    base: &ReviewSession,
    ours: &ReviewSession,
) -> io::Result<(ReviewSession, crate::datafile::Saved)> {
    crate::datafile::with_lock(file, || {
        let merged = match load(file) {
            Some(theirs) => ReviewSession::merged(base, ours, &theirs),
            None => ours.clone(),
        };
        let saved = save(file, &merged)?;
        Ok((merged, saved))
    })
}

/// A comment id no other Gitweft picks at the same time: milliseconds since the epoch ×
/// 1000, plus this process's id (mod 1000).
fn fresh_comment_id() -> u64 {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| {
            u64::try_from(d.as_millis()).unwrap_or(u64::MAX / 1000)
        });
    ms.saturating_mul(1000) + u64::from(std::process::id() % 1000)
}

/// Object ids as hex strings (our gix has no serde feature).
mod hex_id {
    use gitweft_git::ObjectId;
    use serde::{Deserialize, Deserializer, Serializer, de::Error as _};

    pub fn serialize<S: Serializer>(id: &ObjectId, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&id.to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<ObjectId, D::Error> {
        let text = String::deserialize(deserializer)?;
        ObjectId::from_hex(text.as_bytes()).map_err(D::Error::custom)
    }
}

#[cfg(test)]
mod merge_tests {
    use super::*;
    use gitweft_git::gix::hash::Kind;

    fn note(body: &str) -> NewComment {
        NewComment {
            path: "f.rs".into(),
            side: Side::New,
            line: 1,
            anchor: Anchor::default(),
            body: body.into(),
            author: String::new(),
            commit: ObjectId::null(Kind::Sha1),
            created: 0,
        }
    }

    fn bodies(session: &ReviewSession) -> Vec<&str> {
        session.comments.iter().map(|c| c.body.as_str()).collect()
    }

    /// Two instances on one review, each saving from the session it read.
    #[test]
    fn two_instances_keep_each_others_comments_and_marks() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("topic.json");
        let mut base = ReviewSession::new("refs/heads/topic".into(), "refs/heads/main".into());
        let kept = base.add_comment(note("kept"));
        let dropped = base.add_comment(note("dropped by a"));
        let edited = base.add_comment(note("edited by b, dropped by a"));
        save(&file, &base).unwrap();

        // a: drops two comments, adds one, marks a.rs viewed, marks the range reviewed.
        let mut a = base.clone();
        a.remove_comment(dropped);
        a.remove_comment(edited);
        a.add_comment(note("from a"));
        a.set_viewed("a.rs", ObjectId::null(Kind::Sha1), true);
        a.mark_reviewed(ObjectId::null(Kind::Sha1), ObjectId::null(Kind::Sha1));
        // b, from the same start (its new comment may even get a's id): edits two, adds one,
        // marks b.rs viewed.
        let mut b = base.clone();
        b.comment_mut(kept).unwrap().body = "kept, edited by b".into();
        b.comment_mut(edited).unwrap().body = "edited by b".into();
        b.add_comment(note("from b"));
        b.set_viewed("b.rs", ObjectId::null(Kind::Sha1), true);
        save_merged(&file, &base, &b).unwrap();
        let (merged, _) = save_merged(&file, &base, &a).unwrap();

        // b's edit stays though a dropped that comment; a's drop of the untouched one holds
        // (the order of "from a" and "from b" follows their ids).
        let mut sorted = bodies(&merged);
        sorted.sort_unstable();
        assert_eq!(
            sorted,
            ["edited by b", "from a", "from b", "kept, edited by b"]
        );
        let ids: Vec<u64> = merged.comments.iter().map(|c| c.id).collect();
        let mut unique = ids.clone();
        unique.dedup();
        assert_eq!(ids, unique, "every comment keeps an id of its own");
        let mut viewed: Vec<&str> = merged.viewed.iter().map(|m| m.path.as_str()).collect();
        viewed.sort_unstable();
        assert_eq!(viewed, ["a.rs", "b.rs"]);
        assert!(merged.reviewed.is_some());
        assert_eq!(load(&file), Some(merged));
    }

    #[test]
    fn a_session_saved_over_nothing_is_written_as_it_is() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("new.json");
        let base = ReviewSession::new("refs/heads/t".into(), "refs/heads/main".into());
        let mut ours = base.clone();
        ours.add_comment(note("first"));
        let (merged, _) = save_merged(&file, &base, &ours).unwrap();
        assert_eq!(merged, ours);
        assert_eq!(load(&file), Some(ours));
    }
}

#[cfg(test)]
mod anchor_tests {
    use super::*;
    use proptest::prelude::*;

    fn comment_at(lines: &[&str], index: usize) -> NewComment {
        NewComment {
            path: "f.rs".into(),
            side: Side::New,
            line: index as u32 + 1,
            anchor: Anchor::at(lines, index),
            body: "note".into(),
            author: String::new(),
            commit: ObjectId::null(gitweft_git::gix::hash::Kind::Sha1),
            created: 0,
        }
    }

    #[test]
    fn anchors_keep_their_neighbours_up_to_the_file_edges() {
        let lines = ["a", "b", "c", "d", "e"];
        let anchor = Anchor::at(&lines, 0);
        assert_eq!(anchor.text, "a");
        assert!(anchor.before.is_empty());
        assert_eq!(anchor.after, ["b", "c"]);
        let anchor = Anchor::at(&lines, 4);
        assert_eq!(anchor.before, ["c", "d"]);
        assert!(anchor.after.is_empty());
    }

    #[test]
    fn the_best_matching_copy_wins_then_the_nearest() {
        // "x" twice: the second has the original neighbours.
        let before = ["p", "x", "q", "a", "x", "b"];
        let anchor = Anchor::at(&before, 4);
        let after = ["a", "x", "b", "new", "p", "x", "q"];
        assert_eq!(anchor.find(&after, 5), Some(2));
        // Neighbours gone on both copies: the nearer one to the old place.
        let changed = ["x", "1", "2", "3", "x"];
        assert_eq!(anchor.find(&changed, 5), Some(5));
        assert_eq!(anchor.find(&changed, 1), Some(1));
    }

    #[test]
    fn blank_lines_need_a_neighbour_and_trailing_spaces_dont_count() {
        let lines = ["fn f() {", "", "}"];
        let anchor = Anchor::at(&lines, 1);
        assert_eq!(anchor.find(&["x", "", "y"], 2), None);
        assert_eq!(anchor.find(&["", "fn f() {  ", "", "}"], 2), Some(3));
    }

    #[test]
    fn outdated_comments_come_back_when_their_line_returns() {
        let lines = ["one", "two", "three"];
        let mut session = ReviewSession::new("refs/heads/t".into(), "refs/heads/main".into());
        let id = session.add_comment(comment_at(&lines, 1));
        assert!(session.reanchor("f.rs", Side::New, &["one", "three"]));
        assert!(session.comments[0].outdated);
        assert!(!session.reanchor("f.rs", Side::New, &["one", "three"]));
        assert!(session.reanchor("f.rs", Side::New, &["zero", "one", "two", "three"]));
        assert_eq!(
            (session.comments[0].line, session.comments[0].outdated),
            (3, false)
        );
        // Other files and sides are left alone.
        assert!(!session.reanchor("g.rs", Side::New, &[]));
        assert!(!session.reanchor("f.rs", Side::Old, &[]));
        assert_eq!(session.commented(), [("f.rs".to_owned(), Side::New)]);

        let mut stale = session.clone();
        stale.comment_mut(id).unwrap().line = 1;
        assert!(stale.take_positions(&session));
        assert_eq!(stale.comments[0].line, 3);
    }

    proptest! {
        /// With distinct lines, inserting other lines anywhere moves the anchor exactly
        /// with its line; deleting the line makes it outdated.
        #[test]
        fn anchors_follow_their_line(
            count in 1usize..40,
            pick in any::<prop::sample::Index>(),
            inserts in prop::collection::vec((any::<prop::sample::Index>(), 0usize..3), 0..8),
        ) {
            let original: Vec<String> = (0..count).map(|n| format!("line {n}")).collect();
            let lines: Vec<&str> = original.iter().map(String::as_str).collect();
            let index = pick.index(count);
            let anchor = Anchor::at(&lines, index);

            let mut changed = original.clone();
            let mut fresh = 0;
            for (at, how_many) in &inserts {
                let at = at.index(changed.len() + 1);
                for _ in 0..*how_many {
                    changed.insert(at, format!("inserted {fresh}"));
                    fresh += 1;
                }
            }
            let now: Vec<&str> = changed.iter().map(String::as_str).collect();
            let expected = now.iter().position(|line| *line == lines[index]).unwrap() as u32 + 1;
            prop_assert_eq!(anchor.find(&now, index as u32 + 1), Some(expected));

            let without: Vec<&str> = now.iter().copied().filter(|line| *line != lines[index]).collect();
            prop_assert_eq!(anchor.find(&without, index as u32 + 1), None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(byte: u8) -> ObjectId {
        ObjectId::from_bytes_or_panic(&[byte; 20])
    }

    fn comment(body: &str) -> NewComment {
        NewComment {
            path: "src/lib.rs".into(),
            side: Side::New,
            line: 12,
            anchor: Anchor {
                text: "let x = 1;".into(),
                before: vec!["fn f() {".into()],
                after: vec!["}".into()],
            },
            body: body.into(),
            author: String::new(),
            commit: id(9),
            created: 1_700_000_000,
        }
    }

    #[test]
    fn a_deleted_comment_can_be_restored() {
        let mut session = ReviewSession::new("refs/heads/topic".into(), "refs/heads/main".into());
        let first = session.add_comment(comment("first"));
        let second = session.add_comment(comment("second"));
        session.remove_comment(first);
        let ids = |list: &[Comment]| list.iter().map(|c| c.id).collect::<Vec<_>>();
        assert_eq!(ids(&session.comments), [second]);
        assert_eq!(ids(&session.deleted), [first]);
        assert!(session.restore_comment(first));
        assert_eq!(ids(&session.comments), [first, second]);
        assert!(session.deleted.is_empty());
        assert!(!session.restore_comment(first));

        // At most DELETED_KEPT are kept, the newest first.
        for n in 0..DELETED_KEPT + 3 {
            let id = session.add_comment(comment(&format!("c{n}")));
            session.remove_comment(id);
        }
        assert_eq!(session.deleted.len(), DELETED_KEPT);
        assert_eq!(session.deleted[0].body, format!("c{}", DELETED_KEPT + 2));

        // Deleted here while the other side changed nothing: deleted in the merge too.
        let mut base = ReviewSession::new("refs/heads/topic".into(), "refs/heads/main".into());
        let kept = base.add_comment(comment("kept"));
        let mut ours = base.clone();
        ours.remove_comment(kept);
        let merged = ReviewSession::merged(&base, &ours, &base);
        assert!(merged.comments.is_empty());
        assert_eq!(ids(&merged.deleted), [kept]);
    }

    /// Latin-1 names that show alike ("caf\u{fffd}") stay two files, through a save.
    #[test]
    fn paths_that_arent_utf8_keep_their_bytes() {
        let (e_acute, e_grave) = (b"caf\xe9.txt".as_slice(), b"caf\xe8.txt".as_slice());
        let (one, two) = (path_key(e_acute), path_key(e_grave));
        assert_ne!(one, two);
        assert_eq!(path_bytes(&one), e_acute);
        assert_eq!(path_shown(&one), "caf\u{fffd}.txt");
        // A UTF-8 name is itself, as sessions saved before kept it.
        assert_eq!(path_key("café.txt".as_bytes()), "café.txt");
        assert_eq!(path_bytes("café.txt"), "café.txt".as_bytes());

        let mut session = ReviewSession::new("refs/heads/t".into(), "refs/heads/main".into());
        session.set_viewed(&one, id(1), true);
        assert!(session.is_viewed(&one, id(1)));
        assert!(!session.is_viewed(&two, id(1)));

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("t.json");
        save(&file, &session).unwrap();
        let loaded = load(&file).unwrap();
        assert!(loaded.is_viewed(&one, id(1)));
        assert_eq!(path_bytes(&loaded.viewed[0].path), e_acute);
    }

    #[test]
    fn viewed_marks_follow_a_file_renamed_without_changes() {
        let mut session = ReviewSession::new("refs/heads/topic".into(), "refs/heads/main".into());
        session.set_viewed("old.rs", id(1), true);
        session.set_viewed("kept.rs", id(2), true);
        session.set_viewed("a.rs", id(4), true);
        // old.rs is new.rs now, unchanged; kept.rs changed; a.rs became b.rs and changed.
        let files = [
            ("new.rs".to_owned(), id(1)),
            ("kept.rs".to_owned(), id(3)),
            ("b.rs".to_owned(), id(5)),
        ];
        assert!(session.carry_marks(&files));
        assert!(session.is_viewed("new.rs", id(1)));
        assert!(!session.is_viewed("old.rs", id(1)));
        assert!(!session.is_viewed("kept.rs", id(3)));
        assert!(!session.is_viewed("b.rs", id(5)));
        // Carried once: nothing more to carry.
        assert!(!session.carry_marks(&files));
    }

    #[test]
    fn comments_keep_their_author_and_the_export_names_it() {
        let mut session = ReviewSession::new("refs/heads/topic".into(), "refs/heads/main".into());
        let mut new = comment("Rename this");
        new.author = "Ada Lovelace".into();
        session.add_comment(new);
        let place = markdown_place(&session.comments[0], &ExportWords::default());
        assert!(place.contains("by Ada Lovelace"), "{place}");
        // Saved before comments had authors: none, and the export says nothing of one.
        let json = serde_json::to_string(&session)
            .unwrap()
            .replace(r#","author":"Ada Lovelace""#, "");
        let old: ReviewSession = serde_json::from_str(&json).unwrap();
        assert_eq!(old.comments[0].author, "");
        let place = markdown_place(&old.comments[0], &ExportWords::default());
        assert!(!place.contains("by"), "{place}");
    }

    #[test]
    fn viewed_marks_hold_only_for_the_content_they_were_made_at() {
        let mut session = ReviewSession::new("refs/heads/topic".into(), "refs/heads/main".into());
        session.set_viewed("a.txt", id(1), true);
        assert!(session.is_viewed("a.txt", id(1)));
        // The file changed: the mark no longer applies.
        assert!(!session.is_viewed("a.txt", id(2)));
        assert!(!session.is_viewed("b.txt", id(1)));
        // Viewed again at the new content: one mark, the new one.
        session.set_viewed("a.txt", id(2), true);
        assert_eq!(session.viewed.len(), 1);
        assert!(session.is_viewed("a.txt", id(2)));
        session.set_viewed("a.txt", id(2), false);
        assert!(session.viewed.is_empty());
    }

    #[test]
    fn comments_get_fresh_ids() {
        let mut session = ReviewSession::new("refs/heads/topic".into(), "refs/heads/main".into());
        let first = session.add_comment(comment("one"));
        let second = session.add_comment(comment("two"));
        assert_ne!(first, second);
        session.remove_comment(first);
        let third = session.add_comment(comment("three"));
        assert!(third > second, "ids are never reused");
        session.comment_mut(third).unwrap().resolved = true;
        assert!(session.comments.iter().any(|c| c.id == third && c.resolved));
    }

    #[test]
    fn sessions_round_trip_and_other_versions_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("reviews/topic.json");
        assert_eq!(load(&file), None);
        let mut session = ReviewSession::new("refs/heads/topic".into(), "refs/heads/main".into());
        session.reviewed = Some(ReviewedRange {
            base: id(1),
            head: id(2),
        });
        session.set_viewed("a.txt", id(3), true);
        session.add_comment(comment("Why \"1\"?\nSecond line."));
        save(&file, &session).unwrap();
        assert_eq!(load(&file), Some(session));

        let text = std::fs::read_to_string(&file).unwrap();
        std::fs::write(&file, text.replace("\"version\": 1", "\"version\": 2")).unwrap();
        assert_eq!(load(&file), None);
        std::fs::write(&file, "{ not json").unwrap();
        assert_eq!(load(&file), None);
    }

    fn note(path: &str, side: Side, line: u32, text: &str, body: &str) -> NewComment {
        NewComment {
            path: path.into(),
            side,
            line,
            anchor: Anchor {
                text: text.into(),
                ..Anchor::default()
            },
            body: body.into(),
            author: String::new(),
            commit: id(0x23),
            created: 1_700_000_000,
        }
    }

    #[test]
    fn markdown_export_groups_sorts_and_marks_comments() {
        let mut session = ReviewSession::new("refs/heads/topic".into(), "refs/heads/main".into());
        session.mark_reviewed(id(0x0b), id(0x23));
        let outdated = session.add_comment(note(
            "docs/notes.md",
            Side::New,
            40,
            "the line it was on",
            "- one\n- two",
        ));
        session.comment_mut(outdated).unwrap().outdated = true;
        session.add_comment(note(
            "code.txt",
            Side::New,
            12,
            "line 9, changed  \t",
            "Why is this changed?\n\nSecond paragraph of the comment.",
        ));
        session.add_comment(note(
            "docs/notes.md",
            Side::New,
            7,
            "   ",
            "Extra blank line?",
        ));
        let resolved = session.add_comment(note(
            "code.txt",
            Side::Old,
            3,
            "old text",
            "Looks fine now.\n",
        ));
        session.comment_mut(resolved).unwrap().resolved = true;

        let info = ExportInfo {
            head: id(0x23),
            merge_base: Some(id(0x0b)),
        };
        let expected = [
            "# Review of topic",
            "",
            "Against `main` · branch at `23232323` · merge base `0b0b0b0b` · last marked reviewed at `23232323`",
            "",
            "## code.txt",
            "",
            "### Line 3 (old) · resolved",
            "",
            "> old text",
            "",
            "Looks fine now.",
            "",
            "### Line 12 (new)",
            "",
            "> line 9, changed",
            "",
            "Why is this changed?",
            "",
            "Second paragraph of the comment.",
            "",
            "## docs/notes.md",
            "",
            "### Line 7 (new)",
            "",
            "> ",
            "",
            "Extra blank line?",
            "",
            "### Line 40 (new) · outdated",
            "",
            "> the line it was on",
            "",
            "- one",
            "- two",
            "",
        ]
        .join("\n");
        assert_eq!(session.to_markdown(&info), expected);
    }

    #[test]
    fn markdown_export_without_comments_says_so() {
        let session = ReviewSession::new("refs/heads/topic".into(), "refs/heads/main".into());
        let info = ExportInfo {
            head: id(0x23),
            merge_base: Some(id(0x0b)),
        };
        assert_eq!(
            session.to_markdown(&info),
            "# Review of topic\n\n\
             Against `main` · branch at `23232323` · merge base `0b0b0b0b`\n\n\
             No comments.\n"
        );
    }

    #[test]
    fn markdown_export_puts_special_paths_in_code() {
        let mut session = ReviewSession::new("refs/heads/topic".into(), "refs/heads/main".into());
        session.add_comment(note("src/weird_name.rs", Side::New, 1, "x", "Hm."));
        session.add_comment(note("src/plain.rs", Side::New, 1, "x", "Hm."));
        session.add_comment(note("a`b.rs", Side::New, 1, "x", "Hm."));
        let info = ExportInfo {
            head: id(0x23),
            merge_base: None,
        };
        let text = session.to_markdown(&info);
        assert!(text.contains("\n## `src/weird_name.rs`\n"), "{text}");
        assert!(text.contains("\n## src/plain.rs\n"), "{text}");
        assert!(text.contains("\n## ``a`b.rs``\n"), "{text}");
        assert!(text.ends_with("Hm.\n") && !text.ends_with("\n\n"), "{text}");
    }

    #[test]
    fn markdown_export_takes_whole_phrases_in_another_language() {
        let mut session = ReviewSession::new("refs/heads/topic".into(), "refs/heads/main".into());
        session.add_comment(note("a.rs", Side::Old, 7, "x", "Hm."));
        session.comments[0].resolved = true;
        let info = ExportInfo {
            head: id(0x23),
            merge_base: None,
        };
        let words = ExportWords {
            title: "%{branch} — Überprüfung".into(),
            against: "gegen %{base}".into(),
            branch_at: "Zweig bei %{sha}".into(),
            place: "%{side}: Zeile %{line}".into(),
            old: "Alt".into(),
            resolved: "erledigt".into(),
            ..ExportWords::default()
        };
        let text = session.to_markdown_in(&info, &words);
        assert!(
            text.starts_with(
                "# topic — Überprüfung\n\ngegen `main` · Zweig bei `23232323`\n\n## a.rs\n\n\
                 ### Alt: Zeile 7 · erledigt\n"
            ),
            "{text}"
        );
    }

    #[test]
    fn markdown_export_leaves_out_what_it_doesnt_know() {
        let session = ReviewSession::new(
            "refs/remotes/origin/topic".into(),
            "refs/remotes/origin/main".into(),
        );
        let info = ExportInfo {
            head: id(0xab),
            merge_base: None,
        };
        let text = session.to_markdown(&info);
        assert!(
            text.starts_with(
                "# Review of origin/topic\n\nAgainst `origin/main` · branch at `abababab`\n\n"
            ),
            "{text}"
        );
        assert!(!text.contains("merge base") && !text.contains("reviewed"));

        let mut session = session;
        session.mark_reviewed(id(1), id(2));
        let text = session.to_markdown(&info);
        assert!(
            text.contains("branch at `abababab` · last marked reviewed at `02020202`\n"),
            "{text}"
        );
    }

    #[test]
    fn one_file_per_branch() {
        let root = Path::new("/home/u/code/app");
        let a = review_file(root, "refs/heads/feature/x");
        assert_ne!(a, review_file(root, "refs/heads/feature_x"));
        assert_ne!(
            a,
            review_file(Path::new("/home/u/other/app"), "refs/heads/feature/x")
        );
        let name = a.file_name().unwrap().to_string_lossy();
        assert!(
            name.starts_with("feature_x-") && name.ends_with(".json"),
            "{name}"
        );
        assert_eq!(a.parent().unwrap().file_name().unwrap(), "reviews");
    }
}
