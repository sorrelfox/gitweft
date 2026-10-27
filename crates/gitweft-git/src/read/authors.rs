//! People who recently committed to the repository (co-author suggestions).

use std::collections::HashMap;
use std::path::Path;

use crate::cancel::CancelToken;
use crate::cli::{GitCli, GitError};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Person {
    pub name: String,
    pub email: String,
}

impl Person {
    /// `Name <email>`, as used in trailers.
    pub fn ident(&self) -> String {
        format!("{} <{}>", self.name, self.email)
    }
}

/// Distinct authors of the last `commits` commits on any branch, after `.mailmap`, most
/// frequent first (ties: most recent first). An empty repository has none.
pub fn recent_authors(
    git: &GitCli,
    workdir: &Path,
    commits: usize,
    cancel: &CancelToken,
) -> Result<Vec<Person>, GitError> {
    let output = git
        .command(workdir)
        .args(["log", "--all", "--no-show-signature"])
        .arg(format!("-n{commits}"))
        .arg("--format=%aN%x00%aE")
        .read_only()
        .run(cancel)?;
    // Keyed by lower-cased email: the same person with differently cased addresses.
    let mut seen: HashMap<String, (Person, usize, usize)> = HashMap::new();
    for (position, line) in output.stdout_str().lines().enumerate() {
        let Some((name, email)) = line.split_once('\0') else {
            continue;
        };
        if email.is_empty() {
            continue;
        }
        let entry = seen.entry(email.to_lowercase()).or_insert_with(|| {
            let person = Person {
                name: name.to_owned(),
                email: email.to_owned(),
            };
            (person, 0, position)
        });
        entry.1 += 1;
    }
    let mut people: Vec<(Person, usize, usize)> = seen.into_values().collect();
    people.sort_by(|a, b| b.1.cmp(&a.1).then(a.2.cmp(&b.2)));
    Ok(people.into_iter().map(|(person, ..)| person).collect())
}
