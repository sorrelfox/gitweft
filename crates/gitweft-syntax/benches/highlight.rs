//! Highlighting a large file (`just bench`).

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use gitweft_syntax::{highlight_lines, syntax_for_path};

/// `lines` lines of Rust: doc comments, generic functions, strings, numbers and comments.
fn rust_source(lines: usize) -> Vec<String> {
    (0..lines)
        .map(|i| match i % 5 {
            0 => format!("/// What item {i} does."),
            1 => format!("pub fn item_{i}<T: Clone>(value: &T, count: usize) -> Vec<T> {{"),
            2 => format!("    let label = \"item {i}\"; // {i} in a string"),
            3 => "    vec![value.clone(); count]".to_owned(),
            _ => "}".to_owned(),
        })
        .collect()
}

fn highlight(c: &mut Criterion) {
    let source = rust_source(5_000);
    let lines: Vec<&str> = source.iter().map(String::as_str).collect();
    let mut group = c.benchmark_group("highlight");
    group.sample_size(10);
    group.bench_function("5,000 lines of Rust", |b| {
        b.iter(|| {
            let syntax = syntax_for_path("big.rs").expect("a Rust syntax");
            black_box(highlight_lines(syntax, &lines, true))
        })
    });
    group.finish();
}

criterion_group!(benches, highlight);
criterion_main!(benches);
