//! `--headless-smoke`: exercises the repository-loading path without a window and
//! reports where the time went. CI runs it inside a network-less namespace to prove the
//! app works offline.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use gitweft_core::repo::load_repo;
use gitweft_git::{CancelToken, GitCli};

pub fn run(path: Option<PathBuf>) -> ExitCode {
    let path = path.unwrap_or_else(|| PathBuf::from("."));
    let started = Instant::now();
    match summarize(&path) {
        Ok(summary) => {
            println!("{summary}");
            println!("ok in {:.1?}", started.elapsed());
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("gitweft: smoke test failed: {err}");
            ExitCode::FAILURE
        }
    }
}

fn summarize(path: &Path) -> Result<String, Box<dyn Error + Send + Sync>> {
    let git = GitCli::discover()?;
    let loaded = load_repo(path, None, &CancelToken::new())?;
    let repo = loaded.repo.to_thread_local();
    // A bare repository has no working tree to report on (git status would fail).
    let changed = match repo.workdir() {
        Some(workdir) => {
            let status = git
                .command(workdir)
                .args(["status", "--porcelain=v2", "-z"])
                .read_only()
                .run(&CancelToken::new())?;
            let count = status
                .stdout
                .split(|b| *b == 0)
                .filter(|record| matches!(record.first(), Some(b'1' | b'2' | b'u' | b'?')))
                .count();
            count.to_string()
        }
        None => "none (bare repository)".to_owned(),
    };
    let location = repo.workdir().unwrap_or_else(|| repo.git_dir());

    let refs = &loaded.refs;
    let t = loaded.timings;
    Ok(format!(
        "git: {}\nrepository: {}\nhead: {:?}\nbranches: {} local, {} remote\ntags: {}\n\
         commits: {} in {} lanes\nchanged paths: {changed}\n\
         load: open {:.1?}, refs {:.1?}, walk {:.1?} ({}), layout {:.1?}, first metadata {:.1?}",
        git.version(),
        location.display(),
        refs.head,
        refs.local_branches.len(),
        refs.remote_branches.len(),
        refs.tags.len(),
        loaded.graph.rows(),
        loaded.graph.layout.lane_count(),
        t.open,
        t.refs,
        t.walk,
        if t.walk_via_git {
            "git rev-list"
        } else {
            "gix + commit-graph"
        },
        t.layout,
        t.first_meta,
    ))
}
