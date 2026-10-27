//! The graph's commit metadata, packed. On the Linux kernel's 1.48 million commits a
//! `Vec<CommitMeta>` took about 175 MB: 32 bytes a row in a buffer grown by doubling, and a
//! heap allocation for every summary. Here a row is 16 bytes in a buffer sized once, and the
//! summaries of a loaded chunk share one allocation.

use gitweft_git::read::{Author, CommitMeta};
use std::collections::HashMap;
use std::sync::Arc;

/// Commit metadata for rows `0..len()`, filled a chunk at a time.
#[derive(Default)]
pub(super) struct Metas {
    rows: Vec<Row>,
    /// The summaries of each chunk, one after the other.
    texts: Vec<Box<str>>,
    /// The first row of each chunk in `texts`.
    chunk_starts: Vec<usize>,
    /// Every author once, in the order first seen.
    authors: Vec<Arc<Author>>,
    author_index: HashMap<Arc<Author>, u32>,
}

struct Row {
    /// Where the summary starts in its chunk's text; it ends where the next row's starts.
    start: u32,
    author: u32,
    time: i64,
}

/// One row's metadata, borrowed from [`Metas`].
#[derive(Clone, Copy)]
pub(super) struct Meta<'a> {
    pub summary: &'a str,
    pub author: &'a Author,
    /// Committer time, seconds since the Unix epoch.
    pub time: i64,
}

impl Metas {
    /// Room for `rows` rows, so the row buffer never grows past them.
    pub fn with_capacity(rows: usize) -> Self {
        Self {
            rows: Vec::with_capacity(rows),
            ..Self::default()
        }
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Appends the next rows. A chunk's summaries past 4 GB of text would not be addressable;
    /// a chunk that big is split.
    pub fn append(&mut self, metas: Vec<CommitMeta>) {
        let mut pending = metas.into_iter().peekable();
        while pending.peek().is_some() {
            let first_row = self.rows.len();
            let mut text = String::new();
            while let Some(meta) = pending.peek() {
                if !text.is_empty() && text.len() + meta.summary.len() > u32::MAX as usize {
                    break;
                }
                let meta = pending.next().expect("peeked");
                let author = self.author_id(meta.author);
                self.rows.push(Row {
                    start: text.len() as u32,
                    author,
                    time: meta.time,
                });
                text.push_str(&meta.summary);
            }
            self.texts.push(text.into_boxed_str());
            self.chunk_starts.push(first_row);
        }
    }

    pub fn get(&self, row: usize) -> Option<Meta<'_>> {
        let this = self.rows.get(row)?;
        let chunk = self.chunk_starts.partition_point(|&start| start <= row) - 1;
        let text = &self.texts[chunk];
        let chunk_end = self
            .chunk_starts
            .get(chunk + 1)
            .copied()
            .unwrap_or(self.rows.len());
        let end = if row + 1 < chunk_end {
            self.rows[row + 1].start as usize
        } else {
            text.len()
        };
        Some(Meta {
            summary: &text[this.start as usize..end],
            author: &self.authors[this.author as usize],
            time: this.time,
        })
    }

    #[cfg(test)]
    pub fn author_count(&self) -> usize {
        self.authors.len()
    }

    fn author_id(&mut self, author: Arc<Author>) -> u32 {
        if let Some(&id) = self.author_index.get(&author) {
            return id;
        }
        let id = self.authors.len() as u32;
        self.authors.push(Arc::clone(&author));
        self.author_index.insert(author, id);
        id
    }
}

#[cfg(test)]
mod tests {
    use super::Metas;
    use gitweft_git::read::{Author, CommitMeta};
    use std::sync::Arc;

    fn meta(summary: &str, name: &str, time: i64) -> CommitMeta {
        CommitMeta {
            summary: summary.into(),
            author: Arc::new(Author {
                name: name.into(),
                email: format!("{name}@example.com").into(),
            }),
            time,
        }
    }

    /// Each row reads back its own summary, author and time, across chunks, with empty and
    /// non-ASCII summaries at a chunk's ends; each author is kept once.
    #[test]
    fn rows_read_back_what_was_appended_across_chunks() {
        let mut metas = Metas::with_capacity(6);
        metas.append(vec![
            meta("", "ann", 1),
            meta("first", "bob", 2),
            meta("héllo", "ann", 3),
        ]);
        metas.append(vec![meta("again", "bob", 4)]);
        metas.append(vec![]);
        metas.append(vec![meta("ünï", "cy", 5), meta("", "ann", 6)]);
        let read: Vec<(String, String, String, i64)> = (0..metas.len())
            .map(|row| {
                let m = metas.get(row).unwrap();
                (
                    m.summary.to_owned(),
                    m.author.name.to_string(),
                    m.author.email.to_string(),
                    m.time,
                )
            })
            .collect();
        let row = |s: &str, n: &str, t| (s.to_owned(), n.to_owned(), format!("{n}@example.com"), t);
        assert_eq!(
            read,
            vec![
                row("", "ann", 1),
                row("first", "bob", 2),
                row("héllo", "ann", 3),
                row("again", "bob", 4),
                row("ünï", "cy", 5),
                row("", "ann", 6),
            ]
        );
        assert!(metas.get(6).is_none());
        assert_eq!(metas.author_count(), 3);
    }
}
