//! The diff algorithms on a large hunk (`just bench`): the word changes of every
//! removed/added pair, the side-by-side layout, and moved lines.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use gitweft_diff::{LineSide, moved_lines, pair_changed_lines, split_rows, word_changes};

/// `lines` lines of a hunk: a context line, then five removed lines and the same five
/// with one word changed, over and over.
fn edited(lines: usize) -> Vec<(LineSide, String)> {
    let mut hunk = Vec::with_capacity(lines + 11);
    let mut i = 0;
    while hunk.len() < lines {
        hunk.push((
            LineSide::Context,
            format!("    let value_{i} = compute(input, {i});"),
        ));
        for k in 0..5 {
            let line = format!("    total += weight_{k} * sample({i}, {k}); // old");
            hunk.push((LineSide::Removed, line));
        }
        for k in 0..5 {
            let line = format!("    total += weight_{k} * sample({i}, {k} + 1); // new");
            hunk.push((LineSide::Added, line));
        }
        i += 1;
    }
    hunk.truncate(lines);
    hunk
}

/// `lines` lines where every block of 20 removed lines comes back as added lines, the
/// blocks in reverse order: everything moved.
fn moved(lines: usize) -> Vec<(LineSide, String)> {
    let blocks = lines / 40;
    let block = |side: LineSide, b: usize| {
        (0..20).map(move |k| (side, format!("    call_{b}_{k}(argument_{k}, &mut state);")))
    };
    let mut hunk: Vec<(LineSide, String)> = Vec::with_capacity(lines);
    for b in 0..blocks {
        hunk.extend(block(LineSide::Removed, b));
    }
    for b in (0..blocks).rev() {
        hunk.extend(block(LineSide::Added, b));
    }
    hunk
}

fn diff(c: &mut Criterion) {
    let edited = edited(20_000);
    let sides: Vec<LineSide> = edited.iter().map(|(side, _)| *side).collect();
    let edited_lines: Vec<(LineSide, &str)> = edited
        .iter()
        .map(|(side, line)| (*side, line.as_str()))
        .collect();
    let moved = moved(20_000);
    let moved_lines_in: Vec<(LineSide, &str)> = moved
        .iter()
        .map(|(side, line)| (*side, line.as_str()))
        .collect();

    let mut group = c.benchmark_group("diff");
    group.sample_size(20);
    group.bench_function("word changes, 20k-line hunk", |b| {
        b.iter(|| {
            for (old, new) in pair_changed_lines(&sides) {
                black_box(word_changes(&edited[old].1, &edited[new].1));
            }
        })
    });
    group.bench_function("side by side, 20k-line hunk", |b| {
        b.iter(|| black_box(split_rows(&sides)))
    });
    group.bench_function("moved lines, 20k-line hunk", |b| {
        b.iter(|| black_box(moved_lines(&edited_lines)))
    });
    group.bench_function("moved lines, 20k lines all moved", |b| {
        b.iter(|| black_box(moved_lines(&moved_lines_in)))
    });
    group.finish();
}

criterion_group!(benches, diff);
criterion_main!(benches);
