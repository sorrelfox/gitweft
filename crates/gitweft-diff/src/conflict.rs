//! Conflict markers in a file that git left mid-merge: parsing them into regions, and
//! writing the file back with conflicts resolved.
//!
//! Handles git's `merge` style (ours / theirs) and the `diff3` / `zdiff3` styles (with
//! the common ancestor after `|||||||`). Lines keep their own line endings, so CRLF files
//! and a missing final newline survive, and a conflict left unresolved is written back
//! byte for byte. Markers are git's default size of 7 characters, or the size the file's
//! `conflict-marker-size` attribute gives ([`parse_conflicts_sized`], #16).

/// Git's marker size when no `conflict-marker-size` attribute says otherwise.
pub const DEFAULT_MARKER_SIZE: usize = 7;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Region {
    /// Lines that merged cleanly (each with its line ending).
    Clean(Vec<String>),
    Conflict(Conflict),
}

/// One conflicted hunk. All line vectors keep their line endings; the `*_marker` fields
/// are the complete marker lines as git wrote them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub ours_marker: String,
    pub ours: Vec<String>,
    /// `diff3` / `zdiff3` only: the common ancestor's version.
    pub base_marker: Option<String>,
    pub base: Option<Vec<String>>,
    pub separator: String,
    pub theirs: Vec<String>,
    pub theirs_marker: String,
}

impl Conflict {
    /// The text after `<<<<<<< `, e.g. `HEAD`.
    pub fn ours_label(&self) -> &str {
        label(&self.ours_marker, '<')
    }

    /// The text after `>>>>>>> `, e.g. `feature`.
    pub fn theirs_label(&self) -> &str {
        label(&self.theirs_marker, '>')
    }

    /// The conflict exactly as it appears in the file.
    pub fn with_markers(&self) -> String {
        let mut out = self.ours_marker.clone();
        out.extend(self.ours.iter().map(String::as_str));
        if let (Some(marker), Some(base)) = (&self.base_marker, &self.base) {
            out.push_str(marker);
            out.extend(base.iter().map(String::as_str));
        }
        out.push_str(&self.separator);
        out.extend(self.theirs.iter().map(String::as_str));
        out.push_str(&self.theirs_marker);
        out
    }
}

/// How one conflict is resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    Ours,
    Theirs,
    /// Ours, then theirs.
    Both,
    /// Theirs, then ours.
    BothTheirsFirst,
    /// The common ancestor's version (only with `diff3` / `zdiff3` markers).
    Base,
    /// Hand-edited text (line endings included).
    Custom(String),
    /// Some lines of each side, by index into the conflict's `ours` and `theirs`: the picked
    /// ours lines, then the picked theirs lines, each in the file's order.
    Picked {
        ours: Vec<usize>,
        theirs: Vec<usize>,
    },
}

/// Where a line of the resolved file came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// Merged cleanly.
    Clean,
    Ours,
    Theirs,
    Base,
    /// Typed by hand.
    Edited,
    /// A conflict still in the file, markers and all.
    Unresolved,
}

/// A line of the resolved file: its text (with its line ending), where it came from, and
/// which conflict it belongs to (counting conflicts only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultLine {
    pub text: String,
    pub origin: Origin,
    pub conflict: Option<usize>,
}

/// Splits `text` into clean regions and conflicts. An unterminated conflict (markers
/// someone half deleted) is kept as clean text, so nothing is lost.
pub fn parse_conflicts(text: &str) -> Vec<Region> {
    parse_conflicts_sized(text, DEFAULT_MARKER_SIZE)
}

/// [`parse_conflicts`] for markers of `size` characters (`conflict-marker-size`): a run of
/// another length isn't a marker, as for git.
pub fn parse_conflicts_sized(text: &str, size: usize) -> Vec<Region> {
    let [ours, base, separator, theirs] = ['<', '|', '=', '>'].map(|c| c.to_string().repeat(size));
    enum Part {
        Ours,
        Base,
        Theirs,
    }
    let mut regions = Vec::new();
    let mut clean: Vec<String> = Vec::new();
    let mut current: Option<(Conflict, Part)> = None;

    for line in text.split_inclusive('\n') {
        let Some((conflict, part)) = &mut current else {
            if is_marker(line, &ours) {
                if !clean.is_empty() {
                    regions.push(Region::Clean(std::mem::take(&mut clean)));
                }
                let conflict = Conflict {
                    ours_marker: line.to_owned(),
                    ours: Vec::new(),
                    base_marker: None,
                    base: None,
                    separator: String::new(),
                    theirs: Vec::new(),
                    theirs_marker: String::new(),
                };
                current = Some((conflict, Part::Ours));
            } else {
                clean.push(line.to_owned());
            }
            continue;
        };
        match part {
            Part::Ours if is_marker(line, &base) => {
                conflict.base_marker = Some(line.to_owned());
                conflict.base = Some(Vec::new());
                *part = Part::Base;
            }
            Part::Ours | Part::Base if is_separator(line, &separator) => {
                conflict.separator = line.to_owned();
                *part = Part::Theirs;
            }
            Part::Ours => conflict.ours.push(line.to_owned()),
            Part::Base => conflict
                .base
                .get_or_insert_with(Vec::new)
                .push(line.to_owned()),
            Part::Theirs if is_marker(line, &theirs) => {
                conflict.theirs_marker = line.to_owned();
                if let Some((conflict, _)) = current.take() {
                    regions.push(Region::Conflict(conflict));
                }
            }
            Part::Theirs => conflict.theirs.push(line.to_owned()),
        }
    }

    // Markers without an end: give the text back as it was.
    if let Some((conflict, part)) = current {
        let mut partial = conflict.ours_marker.clone();
        partial.extend(conflict.ours.iter().map(String::as_str));
        if let (Some(marker), Some(base)) = (&conflict.base_marker, &conflict.base) {
            partial.push_str(marker);
            partial.extend(base.iter().map(String::as_str));
        }
        if matches!(part, Part::Theirs) {
            partial.push_str(&conflict.separator);
            partial.extend(conflict.theirs.iter().map(String::as_str));
        }
        clean.extend(partial.split_inclusive('\n').map(str::to_owned));
    }
    if !clean.is_empty() {
        regions.push(Region::Clean(clean));
    }
    regions
}

/// The number of conflicts in `regions`.
pub fn conflict_count(regions: &[Region]) -> usize {
    regions
        .iter()
        .filter(|region| matches!(region, Region::Conflict(_)))
        .count()
}

/// Writes the file back: `resolutions[i]` resolves the i-th conflict; `None` leaves it
/// with its markers. Missing entries count as `None`.
pub fn resolve(regions: &[Region], resolutions: &[Option<Resolution>]) -> String {
    resolve_lines(regions, resolutions)
        .into_iter()
        .map(|line| line.text)
        .collect()
}

/// [`resolve`], line by line, with where each line came from. Concatenated, the texts are
/// exactly `resolve`'s output.
pub fn resolve_lines(regions: &[Region], resolutions: &[Option<Resolution>]) -> Vec<ResultLine> {
    let mut out = Vec::new();
    let mut index = 0;
    for region in regions {
        let conflict = match region {
            Region::Clean(lines) => {
                out.extend(lines.iter().map(|text| ResultLine {
                    text: text.clone(),
                    origin: Origin::Clean,
                    conflict: None,
                }));
                continue;
            }
            Region::Conflict(conflict) => conflict,
        };
        let resolution = resolutions.get(index).and_then(Option::as_ref);
        let mut push = |lines: &mut dyn Iterator<Item = &String>, origin: Origin| {
            out.extend(lines.map(|text| ResultLine {
                text: text.clone(),
                origin,
                conflict: Some(index),
            }));
        };
        let markers = conflict.with_markers();
        let unresolved: Vec<String> = markers.split_inclusive('\n').map(str::to_owned).collect();
        match resolution {
            None => push(&mut unresolved.iter(), Origin::Unresolved),
            Some(Resolution::Ours) => push(&mut conflict.ours.iter(), Origin::Ours),
            Some(Resolution::Theirs) => push(&mut conflict.theirs.iter(), Origin::Theirs),
            Some(Resolution::Both) => {
                push(&mut conflict.ours.iter(), Origin::Ours);
                push(&mut conflict.theirs.iter(), Origin::Theirs);
            }
            Some(Resolution::BothTheirsFirst) => {
                push(&mut conflict.theirs.iter(), Origin::Theirs);
                push(&mut conflict.ours.iter(), Origin::Ours);
            }
            // Without a base section, "base" can only mean keeping the markers.
            Some(Resolution::Base) => match &conflict.base {
                Some(base) => push(&mut base.iter(), Origin::Base),
                None => push(&mut unresolved.iter(), Origin::Unresolved),
            },
            Some(Resolution::Custom(text)) => {
                let lines: Vec<String> = text.split_inclusive('\n').map(str::to_owned).collect();
                push(&mut lines.iter(), Origin::Edited);
            }
            Some(Resolution::Picked { ours, theirs }) => {
                let picked = |lines: &'_ [String], picks: &'_ [usize]| -> Vec<String> {
                    lines
                        .iter()
                        .enumerate()
                        .filter(|(ix, _)| picks.contains(ix))
                        .map(|(_, line)| line.clone())
                        .collect()
                };
                push(&mut picked(&conflict.ours, ours).iter(), Origin::Ours);
                push(&mut picked(&conflict.theirs, theirs).iter(), Origin::Theirs);
            }
        }
        index += 1;
    }
    out
}

/// Whether `line` is the marker `marker` (followed by a space, a label or the line end).
fn is_marker(line: &str, marker: &str) -> bool {
    line.strip_prefix(marker).is_some_and(|rest| {
        rest.is_empty()
            || rest.starts_with(' ')
            || rest.starts_with('\n')
            || rest.starts_with("\r\n")
    })
}

/// `=======` (of the marker size) alone on its line.
fn is_separator(line: &str, separator: &str) -> bool {
    line.trim_end_matches(['\n', '\r']) == separator
}

/// The label after a marker line's run of `marker` characters.
fn label(marker_line: &str, marker: char) -> &str {
    marker_line.trim_start_matches(marker).trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markers_of_the_attributes_size_are_found_and_others_are_text() {
        // `conflict-marker-size=10`, as for a file whose text has 7-character runs (#16).
        let text = "top\n<<<<<<<<<< HEAD\nours\n==========\ntheirs\n>>>>>>>>>> feature\n\
                    <<<<<<< not a marker here\n";
        let regions = parse_conflicts_sized(text, 10);
        assert_eq!(conflict_count(&regions), 1);
        let Some(Region::Conflict(conflict)) = regions.get(1) else {
            panic!("{regions:?}");
        };
        assert_eq!(
            (conflict.ours_label(), conflict.theirs_label()),
            ("HEAD", "feature")
        );
        assert_eq!(conflict.ours, ["ours\n"]);
        // Nothing is lost: the file comes back as it was.
        assert_eq!(resolve(&regions, &[]), text);
        // At git's default size the same file has no conflict (a run of 10 isn't one of 7).
        assert_eq!(conflict_count(&parse_conflicts(text)), 0);
    }

    const MERGE_STYLE: &str = "\
fn greeting() -> &str {
<<<<<<< HEAD
    \"hi there\"
=======
    \"hello, world\"
>>>>>>> feature
}
";

    const DIFF3_STYLE: &str = "\
a
<<<<<<< ours
one
||||||| base
zero
=======
two
>>>>>>> theirs
b
";

    fn only_conflict(regions: &[Region]) -> &Conflict {
        regions
            .iter()
            .find_map(|region| match region {
                Region::Conflict(conflict) => Some(conflict),
                Region::Clean(_) => None,
            })
            .expect("a conflict")
    }

    #[test]
    fn merge_style_markers() {
        let regions = parse_conflicts(MERGE_STYLE);
        assert_eq!(regions.len(), 3);
        assert_eq!(conflict_count(&regions), 1);
        let conflict = only_conflict(&regions);
        assert_eq!(conflict.ours_label(), "HEAD");
        assert_eq!(conflict.theirs_label(), "feature");
        assert_eq!(conflict.ours, ["    \"hi there\"\n"]);
        assert_eq!(conflict.theirs, ["    \"hello, world\"\n"]);
        assert_eq!(conflict.base, None);

        let resolved = resolve(&regions, &[Some(Resolution::Theirs)]);
        assert_eq!(
            resolved,
            "fn greeting() -> &str {\n    \"hello, world\"\n}\n"
        );
        let both = resolve(&regions, &[Some(Resolution::Both)]);
        assert!(
            both.contains("\"hi there\"\n    \"hello, world\""),
            "{both}"
        );
        let reversed = resolve(&regions, &[Some(Resolution::BothTheirsFirst)]);
        assert!(
            reversed.contains("\"hello, world\"\n    \"hi there\""),
            "{reversed}"
        );
    }

    #[test]
    fn diff3_markers_carry_the_base() {
        let regions = parse_conflicts(DIFF3_STYLE);
        let conflict = only_conflict(&regions);
        assert_eq!(conflict.base.as_deref(), Some(&["zero\n".to_owned()][..]));
        assert_eq!(resolve(&regions, &[Some(Resolution::Base)]), "a\nzero\nb\n");
        assert_eq!(resolve(&regions, &[Some(Resolution::Ours)]), "a\none\nb\n");
        assert_eq!(
            resolve(
                &regions,
                &[Some(Resolution::Custom("one and two\n".into()))]
            ),
            "a\none and two\nb\n"
        );
    }

    #[test]
    fn unresolved_conflicts_round_trip_exactly() {
        for text in [MERGE_STYLE, DIFF3_STYLE] {
            assert_eq!(resolve(&parse_conflicts(text), &[]), text);
        }
        // Two conflicts, the second resolved.
        let text = format!("{MERGE_STYLE}{DIFF3_STYLE}");
        let regions = parse_conflicts(&text);
        assert_eq!(conflict_count(&regions), 2);
        let out = resolve(&regions, &[None, Some(Resolution::Theirs)]);
        assert!(out.starts_with(MERGE_STYLE), "{out}");
        assert!(out.ends_with("a\ntwo\nb\n"), "{out}");
    }

    #[test]
    fn crlf_and_a_missing_final_newline_survive() {
        let text = "x\r\n<<<<<<< HEAD\r\nours\r\n=======\r\ntheirs\r\n>>>>>>> other\r\nend";
        let regions = parse_conflicts(text);
        assert_eq!(resolve(&regions, &[]), text);
        assert_eq!(
            resolve(&regions, &[Some(Resolution::Ours)]),
            "x\r\nours\r\nend"
        );
    }

    const TWO_LINES: &str = "\
top
<<<<<<< HEAD
o1
o2
o3
=======
t1
t2
>>>>>>> topic
end
";

    #[test]
    fn picked_lines_keep_the_file_order_ours_first() {
        let regions = parse_conflicts(TWO_LINES);
        let pick = |ours: &[usize], theirs: &[usize]| {
            resolve(
                &regions,
                &[Some(Resolution::Picked {
                    ours: ours.to_vec(),
                    theirs: theirs.to_vec(),
                })],
            )
        };
        assert_eq!(pick(&[2, 0], &[1]), "top\no1\no3\nt2\nend\n");
        assert_eq!(pick(&[], &[0]), "top\nt1\nend\n");
        assert_eq!(pick(&[], &[]), "top\nend\n");
        // Indexes past the side's lines are ignored.
        assert_eq!(pick(&[9], &[1, 7]), "top\nt2\nend\n");
    }

    #[test]
    fn result_lines_say_where_they_came_from() {
        let regions = parse_conflicts(TWO_LINES);
        let origins = |resolution: Option<Resolution>| -> Vec<(String, Origin, Option<usize>)> {
            resolve_lines(&regions, &[resolution])
                .into_iter()
                .map(|line| (line.text.trim_end().to_owned(), line.origin, line.conflict))
                .collect()
        };
        let both = origins(Some(Resolution::Both));
        assert_eq!(both[0], ("top".into(), Origin::Clean, None));
        assert_eq!(both[1], ("o1".into(), Origin::Ours, Some(0)));
        assert_eq!(both[4], ("t1".into(), Origin::Theirs, Some(0)));
        assert_eq!(both[6], ("end".into(), Origin::Clean, None));
        let unresolved = origins(None);
        assert_eq!(unresolved.len(), 10);
        assert!(
            unresolved[1..9]
                .iter()
                .all(|line| line.1 == Origin::Unresolved)
        );
        let edited = origins(Some(Resolution::Custom("mine\n".into())));
        assert_eq!(edited[1], ("mine".into(), Origin::Edited, Some(0)));
        // The second of two conflicts is numbered 1.
        let text = format!("{TWO_LINES}{MERGE_STYLE}");
        let lines = resolve_lines(&parse_conflicts(&text), &[None, Some(Resolution::Ours)]);
        let second = lines
            .iter()
            .find(|line| line.origin == Origin::Ours)
            .unwrap();
        assert_eq!(second.conflict, Some(1));
    }

    #[test]
    fn result_lines_concatenate_to_the_resolved_file() {
        for text in [MERGE_STYLE, DIFF3_STYLE, TWO_LINES] {
            let regions = parse_conflicts(text);
            for resolution in [
                None,
                Some(Resolution::Ours),
                Some(Resolution::Theirs),
                Some(Resolution::Both),
                Some(Resolution::BothTheirsFirst),
                Some(Resolution::Base),
                Some(Resolution::Custom("x\ny".into())),
                Some(Resolution::Picked {
                    ours: vec![0],
                    theirs: vec![0],
                }),
            ] {
                let resolutions = [resolution];
                let joined: String = resolve_lines(&regions, &resolutions)
                    .into_iter()
                    .map(|line| line.text)
                    .collect();
                assert_eq!(joined, resolve(&regions, &resolutions));
            }
        }
    }

    #[test]
    fn half_deleted_markers_and_lookalikes_stay_text() {
        let unterminated = "keep\n<<<<<<< HEAD\nours\n=======\ntheirs\n";
        let regions = parse_conflicts(unterminated);
        assert_eq!(conflict_count(&regions), 0);
        assert_eq!(resolve(&regions, &[]), unterminated);

        let lookalikes = "=======\n<<<<<<<< eight is not a marker\n>>>>>>> stray\n";
        let regions = parse_conflicts(lookalikes);
        assert_eq!(conflict_count(&regions), 0);
        assert_eq!(resolve(&regions, &[]), lookalikes);
    }
}
