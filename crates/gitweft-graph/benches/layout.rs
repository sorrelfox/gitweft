//! The lane layout of synthetic histories (`just bench`, #26). The original plan's target:
//! a million commits laid out in under 150 ms.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use gitweft_graph::LayoutBuilder;

/// Commits children first, each with its parents: `parents(i)` for commit `i` of `n`.
fn history(n: u32, parents: impl Fn(u32) -> Vec<u32>) -> Vec<(u32, Vec<u32>)> {
    (0..n).map(|i| (i, parents(i))).collect()
}

/// A mainline with a merged two-commit feature branch every 10 commits (as
/// `tests/layout.rs`'s `layout_one_million_commits`).
fn merges(n: u32) -> Vec<(u32, Vec<u32>)> {
    history(n, |i| {
        if i + 1 >= n {
            vec![]
        } else if i % 10 == 0 && i + 3 < n {
            vec![i + 1, i + 2]
        } else {
            vec![i + 1]
        }
    })
}

/// Long-lived lines side by side: every 7th commit also merges one 97 commits further
/// down, so about 14 lines are open at once (a kernel-like width).
fn wide(n: u32) -> Vec<(u32, Vec<u32>)> {
    history(n, |i| {
        let mut parents = Vec::new();
        if i + 1 < n {
            parents.push(i + 1);
        }
        if i % 7 == 0 && i + 97 < n {
            parents.push(i + 97);
        }
        parents
    })
}

fn layout(c: &mut Criterion) {
    let mut group = c.benchmark_group("layout");
    group.sample_size(10);
    let cases = [
        ("1M commits, a merge every 10", merges(1_000_000)),
        ("100k commits, 14 open lines", wide(100_000)),
    ];
    for (name, commits) in &cases {
        group.bench_function(*name, |b| {
            b.iter(|| {
                let mut builder = LayoutBuilder::with_capacity(commits.len());
                for (id, parents) in commits {
                    builder.push(*id, parents, None);
                }
                black_box(builder.finish())
            })
        });
    }
    group.finish();
}

criterion_group!(benches, layout);
criterion_main!(benches);
