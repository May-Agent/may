//! Hunk matching: places each hunk's old side in the file and builds the
//! patched lines.
//!
//! A hunk's old side (context and deleted lines, in order) must match the file
//! contiguously, at or after the previous hunk's end and after the `@@` anchor
//! line when one is given. Placement has two ordered steps. The exact step
//! compares lines byte for byte: one match applies, several are ambiguous and
//! never fall through to the next step. Only when the exact step finds
//! nothing does the whitespace-free step run: it compares the old side and
//! the file with every whitespace character removed, line breaks included, so
//! a region a formatter rewrapped still matches. Its region always covers
//! whole file lines, and several regions are ambiguous too. The matcher never
//! picks an occurrence on its own.

use std::{borrow::Cow, collections::HashSet, ops::Range};

use crate::{
    error::{Excerpt, ExcerptLine, PatchError},
    parse::{Hunk, HunkLine},
};

/// The step of hunk placement that matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Whitespace {
    /// Byte-for-byte equality of lines.
    Strict,
    /// Equality with every whitespace character removed, line breaks
    /// included, over whole file lines.
    Free,
}

impl Whitespace {
    fn key(self, line: &str) -> Cow<'_, str> {
        match self {
            Self::Strict => Cow::Borrowed(line),
            Self::Free => Cow::Owned(line.split_whitespace().collect()),
        }
    }
}

/// Places `hunks` exactly first and, when the exact step finds nothing,
/// whitespace-free, returning the file lines each hunk's old side occupies
/// and the step that placed them. An ambiguity in either step is returned
/// as it is; the exact step's conflict is returned when both steps fail.
pub fn place_tolerant(
    path: &str,
    file: &[String],
    hunks: &[Hunk],
) -> Result<(Vec<Range<usize>>, Whitespace), PatchError> {
    match place_hunks(path, file, hunks, Whitespace::Strict) {
        Err(conflict @ PatchError::Conflict { .. }) => {
            match place_hunks(path, file, hunks, Whitespace::Free) {
                Err(PatchError::Conflict { .. }) => Err(conflict),
                other => other.map(|placement| (placement, Whitespace::Free)),
            }
        }
        other => other.map(|placement| (placement, Whitespace::Strict)),
    }
}

/// Applies `hunks` with [`place_tolerant`] and returns the patched lines.
pub fn apply_tolerant(
    path: &str,
    file: &[String],
    hunks: &[Hunk],
) -> Result<Vec<String>, PatchError> {
    place_tolerant(path, file, hunks).map(|(placement, _)| build(file, hunks, &placement))
}

/// Where each hunk of an update landed in a file, found without changing it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Located {
    /// The step that placed the hunks together, or `None` when they do not
    /// place together (each hunk is then located on its own).
    pub whitespace: Option<Whitespace>,
    /// One entry per hunk, in order: the lines of the current file its old
    /// side occupies (zero-based, end exclusive; empty for a pure insertion),
    /// or `None` when the hunk is unlocated.
    pub ranges: Vec<Option<Range<usize>>>,
}

/// Locates every hunk of an update in `file` without applying anything.
///
/// This is the matching [`apply_tolerant`] performs, exact first and then
/// whitespace-free: when the hunks place together, every range is the one the
/// patch would change. When they do not place together, each hunk is located
/// on its own and is `None` unless it matches exactly one place.
#[must_use]
pub fn locate_hunks(file: &[String], hunks: &[Hunk]) -> Located {
    if let Ok((placement, whitespace)) = place_tolerant("", file, hunks) {
        return Located {
            whitespace: Some(whitespace),
            ranges: placement.into_iter().map(Some).collect(),
        };
    }
    let ranges = hunks
        .iter()
        .map(|hunk| {
            place_tolerant("", file, std::slice::from_ref(hunk))
                .ok()
                .map(|(mut placement, _)| placement.remove(0))
        })
        .collect();
    Located {
        whitespace: None,
        ranges,
    }
}

/// Finds the one consistent placement of `hunks`: the file lines each
/// occupies.
///
/// The hunks are placed in file order, each at or after the previous hunk's
/// end and after its `@@` anchor line. A placement exists only when exactly
/// one placement of all hunks is consistent, so a hunk that is ambiguous alone
/// is resolved by the hunks after it.
///
/// `path` only labels errors.
fn place_hunks(
    path: &str,
    file: &[String],
    hunks: &[Hunk],
    mode: Whitespace,
) -> Result<Vec<Range<usize>>, PatchError> {
    let keys: Vec<Cow<'_, str>> = file.iter().map(|line| mode.key(line)).collect();
    let olds: Vec<Vec<Cow<'_, str>>> = hunks
        .iter()
        .map(|hunk| {
            hunk.lines
                .iter()
                .filter_map(HunkLine::old_side)
                .map(|line| mode.key(line))
                .collect()
        })
        .collect();
    let mut search = Search {
        file,
        keys: &keys,
        flat: (mode == Whitespace::Free).then(|| Flat::new(&keys)),
        olds: &olds,
        hunks,
        mode,
        solutions: Vec::new(),
        dead: HashSet::new(),
        failure: None,
    };
    search.place(0, 0, &mut Vec::new());

    match search.solutions.as_slice() {
        [] => {
            let (hunk, reason) = search.failure.expect("no solution implies a failed hunk");
            Err(PatchError::Conflict {
                path: path.to_owned(),
                hunk: hunk + 1,
                reason,
            })
        }
        [only] => Ok(only.clone()),
        [first, second, ..] => {
            let index = first
                .iter()
                .zip(second)
                .position(|(a, b)| a != b)
                .expect("distinct solutions differ");
            let cursor = if index == 0 { 0 } else { first[index - 1].end };
            let regions = search
                .candidates(index, cursor)
                .expect("the hunk was placed in both solutions");
            let hunk = index + 1;
            let path = path.to_owned();
            let matches = regions
                .into_iter()
                .map(|region| excerpt(file, region))
                .collect();
            Err(match mode {
                Whitespace::Strict => PatchError::Ambiguous {
                    path,
                    hunk,
                    matches,
                },
                Whitespace::Free => PatchError::AmbiguousIgnoringWhitespace {
                    path,
                    hunk,
                    matches,
                },
            })
        }
    }
}

/// Lines shown above and below a match in an ambiguity error: enough for the
/// agent to copy a distinguishing line into the hunk, few enough to keep the
/// error compact.
const EXCERPT_CONTEXT: usize = 2;

/// The matched `region` (0-based file indices, never empty: an empty old side
/// has a single place and is never ambiguous) with up to [`EXCERPT_CONTEXT`]
/// lines on each side, clipped to the file.
fn excerpt(file: &[String], region: Range<usize>) -> Excerpt {
    debug_assert!(!region.is_empty(), "an ambiguous match covers lines");
    let from = region.start.saturating_sub(EXCERPT_CONTEXT);
    let to = (region.end + EXCERPT_CONTEXT).min(file.len());
    Excerpt {
        first: region.start + 1,
        last: region.end,
        lines: (from..to)
            .map(|index| ExcerptLine {
                number: index + 1,
                text: file[index].clone(),
                matched: region.contains(&index),
            })
            .collect(),
    }
}

/// The file's whitespace-free lines laid end to end, for finding a hunk whose
/// lines were rewrapped.
struct Flat {
    /// Every line's whitespace-free text, concatenated.
    text: String,
    /// `offsets[i]` is where line `i` starts in `text`, and the last entry is
    /// `text.len()`; a line with only whitespace is empty, so offsets repeat.
    offsets: Vec<usize>,
}

impl Flat {
    fn new(keys: &[Cow<'_, str>]) -> Self {
        let mut text = String::with_capacity(keys.iter().map(|key| key.len()).sum());
        let mut offsets = Vec::with_capacity(keys.len() + 1);
        for key in keys {
            offsets.push(text.len());
            text.push_str(key);
        }
        offsets.push(text.len());
        Self { text, offsets }
    }
}

/// Backtracking placement of hunks, stopping at the second solution.
struct Search<'a> {
    file: &'a [String],
    keys: &'a [Cow<'a, str>],
    /// The file laid end to end; present exactly for the whitespace-free step.
    flat: Option<Flat>,
    olds: &'a [Vec<Cow<'a, str>>],
    hunks: &'a [Hunk],
    mode: Whitespace,
    /// Complete placements found so far (file lines per hunk), at most two.
    solutions: Vec<Vec<Range<usize>>>,
    /// `(hunk, cursor)` states known to have no solution below them.
    dead: HashSet<(usize, usize)>,
    /// The furthest hunk that could not be placed, with the reason.
    failure: Option<(usize, String)>,
}

impl Search<'_> {
    /// Where hunk `index` may sit when the previous hunk ended at `cursor`;
    /// `Err` carries the reason when it fits nowhere.
    fn candidates(&self, index: usize, cursor: usize) -> Result<Vec<Range<usize>>, String> {
        let hunk = &self.hunks[index];
        let lower = if let Some(anchor) = &hunk.anchor {
            let anchor_key = self.mode.key(anchor);
            let found = self.keys[cursor..]
                .iter()
                .position(|key| *key == anchor_key);
            let Some(found) = found else {
                return Err(format!(
                    "the anchor line {anchor:?} does not exist at or after file line {}",
                    cursor + 1
                ));
            };
            cursor + found + 1
        } else {
            cursor
        };
        let old = &self.olds[index];
        let at_end = hunk.at_end_of_file;
        let regions = if old.is_empty() {
            // A pure insertion has exactly one place: the end of the file
            // for an end-of-file hunk, otherwise right after the cursor.
            let at = if at_end { self.keys.len() } else { lower };
            std::iter::once(at..at).collect()
        } else if let Some(flat) = &self.flat {
            free_candidates(self.keys, flat, old, lower, at_end)
        } else {
            exact_candidates(self.keys, old, lower, at_end)
        };
        if regions.is_empty() {
            return Err(match self.mode {
                Whitespace::Strict => diagnose(self.file, self.keys, old, lower, hunk),
                Whitespace::Free => format!(
                    "the hunk's context/deleted lines match nowhere at or after file line {} even with all whitespace removed",
                    lower + 1
                ),
            });
        }
        Ok(regions)
    }

    /// Places hunks `index..`; returns whether any solution lies below.
    fn place(&mut self, index: usize, cursor: usize, placed: &mut Vec<Range<usize>>) -> bool {
        if index == self.hunks.len() {
            self.solutions.push(placed.clone());
            return true;
        }
        if self.dead.contains(&(index, cursor)) {
            return false;
        }
        let regions = match self.candidates(index, cursor) {
            Ok(regions) => regions,
            Err(reason) => {
                if self
                    .failure
                    .as_ref()
                    .is_none_or(|(deepest, _)| index > *deepest)
                {
                    self.failure = Some((index, reason));
                }
                self.dead.insert((index, cursor));
                return false;
            }
        };
        let mut found = false;
        for region in regions {
            let end = region.end;
            placed.push(region);
            found |= self.place(index + 1, end, placed);
            placed.pop();
            if self.solutions.len() >= 2 {
                return true;
            }
        }
        if !found {
            self.dead.insert((index, cursor));
        }
        found
    }
}

/// Builds the patched lines from a consistent placement.
///
/// A region whose file lines correspond one to one with the hunk's old side
/// (equal once whitespace is removed, which every exact match is) keeps the
/// file's own text for context lines. Any other region, one a formatter
/// rewrapped, is replaced by the hunk's new side as written.
fn build(file: &[String], hunks: &[Hunk], placement: &[Range<usize>]) -> Vec<String> {
    let mut out = Vec::with_capacity(file.len());
    let mut cursor = 0;
    for (hunk, region) in hunks.iter().zip(placement) {
        out.extend_from_slice(&file[cursor..region.start]);
        let old: Vec<&str> = hunk.lines.iter().filter_map(HunkLine::old_side).collect();
        let region_lines = &file[region.clone()];
        let corresponds = region_lines.len() == old.len()
            && region_lines
                .iter()
                .zip(&old)
                .all(|(line, wanted)| same_without_whitespace(line, wanted));
        if corresponds {
            let mut at = region.start;
            for line in &hunk.lines {
                match line {
                    HunkLine::Context(_) => {
                        out.push(file[at].clone());
                        at += 1;
                    }
                    HunkLine::Delete(_) => at += 1,
                    HunkLine::Insert(text) => out.push(text.clone()),
                }
            }
        } else {
            for line in &hunk.lines {
                match line {
                    HunkLine::Context(text) | HunkLine::Insert(text) => out.push(text.clone()),
                    HunkLine::Delete(_) => {}
                }
            }
        }
        cursor = region.end;
    }
    out.extend_from_slice(&file[cursor..]);
    out
}

/// Whether `a` and `b` are the same text once every whitespace is removed.
fn same_without_whitespace(a: &str, b: &str) -> bool {
    a.split_whitespace()
        .flat_map(str::bytes)
        .eq(b.split_whitespace().flat_map(str::bytes))
}

/// Every place at or after `lower` where `old` matches `keys` line for line.
///
/// `old` is never empty: a pure insertion has its one place decided by the
/// caller.
fn exact_candidates(
    keys: &[Cow<'_, str>],
    old: &[Cow<'_, str>],
    lower: usize,
    at_end: bool,
) -> Vec<Range<usize>> {
    let Some(last) = keys.len().checked_sub(old.len()) else {
        return Vec::new();
    };
    (lower..=last)
        .filter(|&start| keys[start..start + old.len()] == *old)
        .filter(|&start| !at_end || start + old.len() == keys.len())
        .map(|start| start..start + old.len())
        .collect()
}

/// Every run of whole file lines at or after `lower` whose text, with line
/// breaks removed, is `old` with line breaks removed (`old` and `keys` hold
/// whitespace-free lines).
///
/// A run starts and ends on a line with text. The old side's own blank lines
/// at its edges take the blank file lines next to the run, as many as the old
/// side has, so a run keeps the blank lines the hunk spells out. `old` is
/// never empty; an old side of blank lines only has no place.
fn free_candidates(
    keys: &[Cow<'_, str>],
    flat: &Flat,
    old: &[Cow<'_, str>],
    lower: usize,
    at_end: bool,
) -> Vec<Range<usize>> {
    let leading = old.iter().take_while(|key| key.is_empty()).count();
    let trailing = old.iter().rev().take_while(|key| key.is_empty()).count();
    if leading == old.len() {
        return Vec::new();
    }
    let core: String = old[leading..old.len() - trailing]
        .iter()
        .map(Cow::as_ref)
        .collect();
    let mut found = Vec::new();
    for first in lower..keys.len() {
        let from = flat.offsets[first];
        if flat.offsets[first + 1] == from || !flat.text[from..].starts_with(&core) {
            continue;
        }
        // The run must end exactly where a line ends: the first line whose
        // end offset is where the core ends is its last line plus one.
        let end = from + core.len();
        let after = flat.offsets.partition_point(|&offset| offset < end);
        if flat.offsets.get(after) != Some(&end) {
            continue;
        }
        let before = keys[lower..first]
            .iter()
            .rev()
            .take(leading)
            .take_while(|key| key.is_empty())
            .count();
        let behind = keys[after..]
            .iter()
            .take(trailing)
            .take_while(|key| key.is_empty())
            .count();
        let region = first - before..after + behind;
        if !at_end || region.end == keys.len() {
            found.push(region);
        }
    }
    found
}

/// Explains why `old` fits nowhere: names the first line of the best partial
/// match that did not match.
fn diagnose(
    file: &[String],
    keys: &[Cow<'_, str>],
    old: &[Cow<'_, str>],
    lower: usize,
    hunk: &Hunk,
) -> String {
    let old_lines: Vec<&str> = hunk.lines.iter().filter_map(HunkLine::old_side).collect();
    let mut best = (0, lower);
    for start in lower..keys.len() {
        let matched = keys[start..]
            .iter()
            .zip(old)
            .take_while(|(key, wanted)| key == wanted)
            .count();
        if matched > best.0 {
            best = (matched, start);
        }
    }
    let (matched, start) = best;
    if matched == old.len() {
        return format!(
            "the context/deleted lines match at file line {} but `*** End of File` requires them to end the file",
            start + 1
        );
    }
    let wanted = old_lines[matched];
    match file.get(start + matched) {
        Some(found) if matched > 0 => format!(
            "context/deleted line {} of the hunk, {wanted:?}, does not match; the closest candidate starts at file line {} and has {found:?} there",
            matched + 1,
            start + 1,
        ),
        _ => format!(
            "the first context/deleted line, {wanted:?}, does not exist at or after file line {}",
            lower + 1
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{Whitespace, apply_tolerant, build, place_hunks};
    use crate::parse::{Action, Hunk, parse};

    fn hunks_of(hunks: &str) -> Vec<Hunk> {
        let patch = format!("*** Begin Patch\n*** Update File: f\n{hunks}\n*** End Patch");
        let Action::Update { hunks, .. } = parse(&patch).expect("parses").remove(0) else {
            panic!("expected update");
        };
        hunks
    }

    fn lines_of(file: &str) -> Vec<String> {
        file.lines().map(String::from).collect()
    }

    fn run(file: &str, hunks: &str, mode: Whitespace) -> Result<String, String> {
        let hunks = hunks_of(hunks);
        let lines = lines_of(file);
        place_hunks("f", &lines, &hunks, mode)
            .map(|placement| build(&lines, &hunks, &placement).join("\n"))
            .map_err(|error| error.to_string())
    }

    /// The two-step placement the tool performs.
    fn tolerant(file: &str, hunks: &str) -> Result<String, String> {
        apply_tolerant("f", &lines_of(file), &hunks_of(hunks))
            .map(|lines| lines.join("\n"))
            .map_err(|error| error.to_string())
    }

    #[test]
    fn applies_several_changes_separated_by_context() {
        let out = run(
            "a\nb\nc\nd\ne",
            "@@\n a\n-b\n+B\n c\n-d\n+D\n e",
            Whitespace::Strict,
        );
        assert_eq!(out.as_deref(), Ok("a\nB\nc\nD\ne"));
    }

    #[test]
    fn applies_hunks_in_file_order() {
        let out = run(
            "1\n2\n3\n4\n5",
            "@@\n-2\n+two\n@@\n-4\n+four",
            Whitespace::Strict,
        );
        assert_eq!(out.as_deref(), Ok("1\ntwo\n3\nfour\n5"));
    }

    #[test]
    fn anchor_selects_a_later_occurrence() {
        let file = "fn a\nx\nfn b\nx";
        let out = run(file, "@@ fn b\n-x\n+y", Whitespace::Strict);
        assert_eq!(out.as_deref(), Ok("fn a\nx\nfn b\ny"));
        let unanchored = run(file, "@@\n-x\n+y", Whitespace::Strict).unwrap_err();
        assert!(unanchored.contains("Ambiguous patch"), "{unanchored}");
    }

    #[test]
    fn later_hunk_resolves_an_ambiguous_earlier_one() {
        // `-x` alone matches lines 1 and 3, but `-a` can only follow the first.
        let out = run("x\na\nx\nb", "@@\n-x\n+X\n@@\n-a\n+A", Whitespace::Strict);
        assert_eq!(out.as_deref(), Ok("X\nA\nx\nb"));
    }

    #[test]
    fn two_consistent_placements_are_ambiguous() {
        let error = run(
            "x
x
x",
            "@@
-x
+X
@@
-x
+Y",
            Whitespace::Strict,
        )
        .unwrap_err();
        assert!(error.contains("Ambiguous patch"), "{error}");
        assert!(error.contains("hunk 2"), "{error}");
    }

    #[test]
    fn ambiguity_lists_every_place() {
        let error = run("s\ns\ns", "@@\n-s\n+u", Whitespace::Strict).unwrap_err();
        assert!(error.contains("matches at 3 places"), "{error}");
        assert!(error.contains("match 1, file lines 1-1"), "{error}");
        assert!(error.contains("match 3, file lines 3-3"), "{error}");
    }

    const GO_FILE: &str = "func (c *Client) Get(id string) error {
\tc.mu.Lock()
\tdefer c.mu.Unlock()
\treturn c.fetch(id)
}
// Delete removes the entry.
func (c *Client) Delete(id string) error {
\tc.mu.Lock()
\tdefer c.mu.Unlock()
\treturn c.remove(id)
}";

    #[test]
    fn ambiguity_shows_numbered_lines_around_each_match() {
        let error = run(
            GO_FILE,
            "@@\n \tc.mu.Lock()\n-\tdefer c.mu.Unlock()\n+\tdefer c.mu.RUnlock()",
            Whitespace::Strict,
        )
        .unwrap_err();
        let expected = "Ambiguous patch: in f: hunk 1 matches at 2 places; add surrounding context or an `@@ anchor` line to select one. Lines marked `>` are the matched ones:

match 1, file lines 2-3:
  1| func (c *Client) Get(id string) error {
> 2| \tc.mu.Lock()
> 3| \tdefer c.mu.Unlock()
  4| \treturn c.fetch(id)
  5| }

match 2, file lines 8-9:
  6| // Delete removes the entry.
  7| func (c *Client) Delete(id string) error {
> 8| \tc.mu.Lock()
> 9| \tdefer c.mu.Unlock()
  10| \treturn c.remove(id)
  11| }
";
        assert_eq!(error, expected);
    }

    #[test]
    fn ambiguity_excerpt_is_clipped_at_file_start_and_end() {
        let error = run("x\ny\nz\nq\nx", "@@\n-x\n+X", Whitespace::Strict).unwrap_err();
        assert!(error.contains("> 1| x\n  2| y\n  3| z\n"), "{error}");
        assert!(error.contains("  3| z\n  4| q\n> 5| x\n"), "{error}");
        assert!(!error.contains("0|") && !error.contains("6|"), "{error}");
    }

    #[test]
    fn ambiguity_excerpt_keeps_cjk_lines_whole() {
        let file = "fn 日本() {\n    // 你好，世界\n    保存();\n}\n\nfn 한국() {\n    // 안녕하세요\n    保存();\n}";
        let error = run(
            file,
            "@@\n-    保存();\n+    保存する();",
            Whitespace::Strict,
        )
        .unwrap_err();
        assert!(
            error.contains("  2|     // 你好，世界\n> 3|     保存();\n  4| }\n"),
            "{error}"
        );
        assert!(
            error.contains("  7|     // 안녕하세요\n> 8|     保存();\n  9| }"),
            "{error}"
        );
    }

    #[test]
    fn conflict_names_the_hunk_and_the_first_unmatched_line() {
        let error = run(
            "a\nb\nc\nd",
            "@@\n-a\n+A\n@@\n c\n-nope\n+x",
            Whitespace::Strict,
        )
        .unwrap_err();
        assert!(error.contains("hunk 2"), "{error}");
        assert!(error.contains("\"nope\""), "{error}");
    }

    #[test]
    fn whitespace_free_mode_ignores_whitespace_differences() {
        assert!(run("  a  \nb", "@@\n a\n-b\n+B", Whitespace::Strict).is_err());
        let out = run("  a  \nb", "@@\n a\n-b\n+B", Whitespace::Free);
        assert_eq!(out.as_deref(), Ok("  a  \nB"));
    }

    #[test]
    fn end_of_file_hunk_must_reach_the_end() {
        let out = run("x\ny\nx", "@@\n-x\n+z\n*** End of File", Whitespace::Strict);
        assert_eq!(out.as_deref(), Ok("x\ny\nz"));
        let error = run("x\ny", "@@\n-x\n+z\n*** End of File", Whitespace::Strict).unwrap_err();
        assert!(error.contains("End of File"), "{error}");
    }

    #[test]
    fn exact_match_applies_without_trying_whitespace_free() {
        let out = tolerant("f(a, b)\ng(a,b)", "@@\n-f(a, b)\n+F()");
        assert_eq!(out.as_deref(), Ok("F()\ng(a,b)"));
    }

    #[test]
    fn exact_ambiguity_never_falls_back_to_whitespace_free() {
        // The old side matches lines 1 and 3 exactly; the whitespace-free
        // step would have found line 2 as well, or a unique place had the
        // exact text been absent.
        let error = tolerant("x\n x\nx", "@@\n-x\n+X").unwrap_err();
        assert!(error.starts_with("Ambiguous patch: in f: hunk 1 matches at 2 places"));
        assert!(!error.contains("whitespace"), "{error}");
    }

    #[test]
    fn whitespace_free_keeps_the_file_text_of_context_lines() {
        let out = tolerant("  a  \n\tb\nc", "@@\n a\n-b\n+B\n c");
        assert_eq!(out.as_deref(), Ok("  a  \nB\nc"));
    }

    #[test]
    fn whitespace_free_match_after_a_formatter_changed_spacing() {
        let file = "fn main() {\n    run(a, b);\n}";
        let out = tolerant(file, "@@\n-    run(a,b);\n+    run(&a, &b);");
        assert_eq!(out.as_deref(), Ok("fn main() {\n    run(&a, &b);\n}"));
    }

    #[test]
    fn whitespace_free_match_after_a_formatter_rewrapped_the_lines() {
        let file = "fn main() {\n    call(\n        alpha,\n        beta,\n    );\n    done();\n}";
        let hunk =
            "@@\n fn main() {\n-    call(alpha, beta,);\n+    call(alpha, gamma);\n     done();";
        // The rewrapped call does not correspond line for line, so the
        // matched lines are replaced by the hunk's new side as written.
        let out = tolerant(file, hunk);
        assert_eq!(
            out.as_deref(),
            Ok("fn main() {\n    call(alpha, gamma);\n    done();\n}")
        );
    }

    #[test]
    fn whitespace_free_match_spans_blank_lines_in_the_file() {
        let file = "a();\n\nb();\n\n\nc();";
        let out = tolerant(file, "@@\n-a();b();\n+ab();");
        assert_eq!(out.as_deref(), Ok("ab();\n\n\nc();"));
    }

    #[test]
    fn whitespace_free_region_covers_whole_lines_only() {
        // `ab` ends inside the line `abc`, so there is no whole-line region.
        let error = tolerant("a\nabc", "@@\n-a b\n+x").unwrap_err();
        assert!(error.contains("Patch conflict"), "{error}");
        let error = tolerant("xa\nb", "@@\n-a b\n+x").unwrap_err();
        assert!(error.contains("Patch conflict"), "{error}");
    }

    #[test]
    fn whitespace_free_ambiguity_has_its_own_error() {
        let error = tolerant("f(a,b)\nmid\nf(a,  b)\n", "@@\n-f(a, b)\n+F()").unwrap_err();
        let expected = "Ambiguous patch: in f: hunk 1: the exact text was not found, and the text with all whitespace removed matches at 2 places; add surrounding context or an `@@ anchor` line to select one. Lines marked `>` are the matched ones:

match 1, file lines 1-1:
> 1| f(a,b)
  2| mid
  3| f(a,  b)

match 2, file lines 3-3:
  1| f(a,b)
  2| mid
> 3| f(a,  b)
";
        assert_eq!(error, expected);
    }

    #[test]
    fn whitespace_free_overlapping_regions_are_both_found() {
        let error = tolerant("x\nx\nx", "@@\n-xx\n+y").unwrap_err();
        assert!(
            error.contains("all whitespace removed matches at 2 places"),
            "{error}"
        );
        assert!(error.contains("match 1, file lines 1-2"), "{error}");
        assert!(error.contains("match 2, file lines 2-3"), "{error}");
    }

    #[test]
    fn nothing_matching_even_without_whitespace_is_the_exact_conflict() {
        let error = tolerant("a\nb", "@@\n-c d\n+x").unwrap_err();
        assert!(
            error.starts_with("Patch conflict: in f: hunk 1:"),
            "{error}"
        );
        assert!(error.contains("\"c d\""), "{error}");
    }

    #[test]
    fn whitespace_free_anchor_ignores_whitespace() {
        let file = "fn  a ( )\nx\nfn b()\nx";
        let out = tolerant(file, "@@ fn b ( )\n-x\n+y");
        assert_eq!(out.as_deref(), Ok("fn  a ( )\nx\nfn b()\ny"));
    }

    #[test]
    fn whitespace_free_later_hunk_disambiguates_an_earlier_one() {
        let file = "g(1,2)\nm()\ng(1,2)\nk()";
        let out = tolerant(file, "@@\n-g(1, 2)\n+G()\n@@\n-m( )\n+M()");
        assert_eq!(out.as_deref(), Ok("G()\nM()\ng(1,2)\nk()"));
        // Without the later hunk both places stay consistent.
        let error = tolerant(file, "@@\n-g(1, 2)\n+G()\n@@\n-k( )\n+K()").unwrap_err();
        assert!(
            error.contains("hunk 1: the exact text was not found"),
            "{error}"
        );
    }

    #[test]
    fn whitespace_free_end_of_file_hunk_must_reach_the_end() {
        let out = tolerant("x(1,2)\ny\nx(1,2)", "@@\n-x(1, 2)\n+z\n*** End of File");
        assert_eq!(out.as_deref(), Ok("x(1,2)\ny\nz"));
    }

    #[test]
    fn whitespace_free_match_over_cjk_text() {
        let file = "// 你好\n保存 (日本,  한국);\n// 完了";
        let out = tolerant(file, "@@\n-保存(日本, 한국);\n+保存する();");
        assert_eq!(out.as_deref(), Ok("// 你好\n保存する();\n// 完了"));
    }
}
