//! Commits whose changes add or remove some text ("pickaxe": `git log -S`/`-G`), streamed
//! as git finds them. Searching every diff of a long history takes a while, so the caller
//! shows the results as they come and can cancel.

use std::path::Path;

use gix::ObjectId;

use super::ReadError;
use crate::cancel::CancelToken;
use crate::cli::GitCli;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pickaxe {
    /// `-S`: commits that change how many times the text occurs.
    Text(String),
    /// `-G`: commits with an added or removed line matching the regex.
    Regex(String),
}

impl Pickaxe {
    /// `/regex/` is a regex; anything else is literal text. `None` for an empty query.
    pub fn parse(query: &str) -> Option<Self> {
        let query = query.trim();
        if let Some(regex) = query
            .strip_prefix('/')
            .and_then(|rest| rest.strip_suffix('/'))
            .filter(|regex| !regex.is_empty())
        {
            return Some(Self::Regex(regex.to_owned()));
        }
        (!query.is_empty()).then(|| Self::Text(query.to_owned()))
    }
}

/// Searches the changes of every commit reachable from any ref, newest first, handing
/// the ids to `on_ids` in batches as git finds them.
pub fn pickaxe(
    git: &GitCli,
    cwd: &Path,
    query: &Pickaxe,
    cancel: &CancelToken,
    mut on_ids: impl FnMut(Vec<ObjectId>),
) -> Result<(), ReadError> {
    // Attached (`-Sfoo`), so a query starting with `-` can't be taken for an option.
    let option = match query {
        Pickaxe::Text(text) => format!("-S{text}"),
        Pickaxe::Regex(regex) => format!("-G{regex}"),
    };
    let mut partial: Vec<u8> = Vec::new();
    let mut failed: Option<ReadError> = None;
    git.command(cwd)
        .args(["log", "--all", "--format=%H", "--no-show-signature"])
        .arg(option)
        .arg("--")
        .read_only()
        .run_streaming(cancel, |bytes| {
            partial.extend_from_slice(bytes);
            let end = partial
                .iter()
                .rposition(|b| *b == b'\n')
                .map_or(0, |last| last + 1);
            let mut ids = Vec::new();
            for line in partial[..end]
                .split(|b| *b == b'\n')
                .filter(|l| !l.is_empty())
            {
                match ObjectId::from_hex(line) {
                    Ok(id) => ids.push(id),
                    Err(err) if failed.is_none() => failed = Some(err.into()),
                    Err(_) => {}
                }
            }
            partial.drain(..end);
            if !ids.is_empty() {
                on_ids(ids);
            }
        })?;
    match failed {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::Pickaxe;

    #[test]
    fn slashes_make_a_regex() {
        assert_eq!(Pickaxe::parse("/a.b/"), Some(Pickaxe::Regex("a.b".into())));
        assert_eq!(
            Pickaxe::parse("  needle "),
            Some(Pickaxe::Text("needle".into()))
        );
        // An empty regex or an unclosed one is plain text.
        assert_eq!(Pickaxe::parse("//"), Some(Pickaxe::Text("//".into())));
        assert_eq!(Pickaxe::parse("/x"), Some(Pickaxe::Text("/x".into())));
        assert_eq!(Pickaxe::parse("   "), None);
    }
}
