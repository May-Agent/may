//! Applies a parsed stateless per-line hashline patch to files under a base
//! directory.
//!
//! Line numbers in a patch always address the original file: edits are
//! compiled against the original lines and applied in one pass, so applied
//! hunks never shift the numbers of the ops that follow. Every line anchor in
//! every section is verified against live disk contents before any write lands,
//! so a single mismatch refuses the whole patch atomically with live context
//! and moved-line hints.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::block::BlockResolver;
use crate::error::Error;
use crate::parse::{self, LineAnchor, Op, PasteTarget, Range, Section};
use crate::{format_hash, hashed_line, line_hash, section_header, split_display_lines};

/// Surrounding hashed context lines echoed around each edited hunk.
const ECHO_CONTEXT: usize = 2;
/// Maximum lines echoed in full for a single merged hunk window before eliding.
const ECHO_LIMIT: usize = 40;
/// Head lines shown when a single merged hunk window exceeds [`ECHO_LIMIT`].
const ECHO_HEAD: usize = 25;
/// Tail lines shown when a single merged hunk window exceeds [`ECHO_LIMIT`].
const ECHO_TAIL: usize = 10;
/// Surrounding live context lines shown around each mismatched anchor.
const MISMATCH_CONTEXT: usize = 3;
/// Surrounding live context lines shown around each hinted moved line.
const MOVED_HINT_CONTEXT: usize = 2;
/// Maximum live context lines printed in a hash-mismatch refusal.
const MAX_MISMATCH_CONTEXT_LINES: usize = 40;

/// The result of applying a whole patch.
#[derive(Debug)]
pub struct ApplyReport {
    /// One report per unique touched file, in first-seen patch order.
    pub sections: Vec<SectionReport>,
}

/// What one section's file became.
#[derive(Debug)]
pub struct SectionReport {
    /// The section's path relative to the base directory.
    pub path: PathBuf,
    /// What happened to the file.
    pub outcome: FileOutcome,
    /// The overall 1-indexed inclusive span covering first hunk start through
    /// last hunk end in the new numbering, when the outcome was a change.
    pub changed: Option<Range>,
    /// Individual 1-indexed inclusive new-numbering ranges for each distinct
    /// edited hunk.
    pub hunks: Vec<Range>,
    /// Net line-count change (`new_lines.len() as isize - old_lines.len() as isize`).
    pub line_delta: isize,
}

/// The file-level result of one section.
#[derive(Debug)]
pub enum FileOutcome {
    /// The file's content changed.
    Modified,
    /// The ops applied but produced the same content.
    Unchanged,
    /// `REM` removed the file.
    Removed,
    /// `MV` moved the file; edits landed on the source first.
    Moved {
        /// The destination path relative to the base directory.
        dest: PathBuf,
    },
}

/// Mutable registers shared by the ops of one patch, in patch order.
#[derive(Default)]
struct Clipboard {
    /// Lines captured by the latest unlabeled `CUT`.
    anon: Option<Vec<String>>,
    /// How many unlabeled `CUT`s have not been pasted since.
    pending_anon: usize,
    /// Lines captured into named registers via `YANK` or `CUT … @name`.
    named: HashMap<String, Vec<String>>,
}

impl Clipboard {
    /// Records the lines a `CUT` or `YANK` captured.
    fn capture(&mut self, register: Option<&str>, lines: Vec<String>) {
        match register {
            Some(name) => {
                self.named.insert(name.to_string(), lines);
            }
            None => {
                self.anon = Some(lines);
                self.pending_anon += 1;
            }
        }
    }

    /// Reads the register a `PASTE` or `PUT` paste names.
    fn paste(&mut self, path: &str, register: Option<&str>) -> Result<Vec<String>, Error> {
        match register {
            Some(name) => self
                .named
                .get(name)
                .cloned()
                .ok_or_else(|| Error::EmptyRegister {
                    path: PathBuf::from(path),
                    name: name.to_string(),
                }),
            None => {
                if self.pending_anon != 1 {
                    return Err(Error::Anchoring {
                        path: PathBuf::from(path),
                        detail: if self.pending_anon == 0 {
                            "an unlabeled `PUT` paste needs one unlabeled `CUT` ahead of it"
                                .to_string()
                        } else {
                            "several unlabeled `CUT`s precede this paste — name the register with `YANK R …` / `PASTE R …` or `CUT … @R` / `PUT … @R`"
                                .to_string()
                        },
                    });
                }
                self.pending_anon = 0;
                Ok(self.anon.clone().unwrap_or_default())
            }
        }
    }
}

/// The edits one file's ops compile to, on the original line numbering.
#[derive(Default)]
struct FileEdits {
    /// Original lines (1-indexed) the ops delete.
    deleted: BTreeSet<usize>,
    /// Lines to emit at each insertion gap (0 = before line 1, len = at end).
    inserts: BTreeMap<usize, Vec<String>>,
    /// `REM`.
    remove: bool,
    /// `MV dest`.
    move_to: Option<String>,
}

impl FileEdits {
    /// Marks an original range deleted, refusing overlaps between ops.
    fn delete(&mut self, path: &str, range: Range) -> Result<(), Error> {
        for line in range.start..=range.end {
            if !self.deleted.insert(line) {
                return Err(Error::Anchoring {
                    path: PathBuf::from(path),
                    detail: format!(
                        "two hunks touch line {line} — tighten the ranges so they do not overlap"
                    ),
                });
            }
        }
        Ok(())
    }

    /// Queues insertion lines at an original-numbering gap.
    fn insert(&mut self, gap: usize, lines: impl IntoIterator<Item = String>) {
        self.inserts.entry(gap).or_default().extend(lines);
    }

    /// Whether an op other than `REM`/`MV`/`YANK` mutated content.
    fn has_content_ops(&self) -> bool {
        !self.deleted.is_empty() || !self.inserts.is_empty()
    }
}

/// One file's state while a patch is being verified, compiled, and applied.
struct FileState {
    /// Original text, whole.
    text: String,
    /// Normalized display lines (`\r` stripped, trailing `\n` omitted).
    lines: Vec<String>,
    /// Original line separator (`"\r\n"` when the file uses CRLF, else `"\n"`).
    line_ending: &'static str,
    /// Whether post-edit non-empty text should end with [`Self::line_ending`].
    final_newline: bool,
    /// Compiled edits on the original numbering.
    edits: FileEdits,
}

impl FileState {
    /// Loads `abs_path` for section `path`.
    fn load(abs_path: &Path, path: &str) -> Result<Self, Error> {
        let bytes = std::fs::read(abs_path).map_err(|source| Error::Io {
            path: abs_path.to_path_buf(),
            source,
        })?;
        let text = String::from_utf8(bytes).map_err(|_| Error::File {
            path: PathBuf::from(path),
            detail: "not UTF-8 text — hashline edits text files".to_string(),
        })?;
        let line_ending = if text.contains("\r\n") { "\r\n" } else { "\n" };
        let final_newline = text.is_empty() || text.ends_with('\n');
        let lines = split_display_lines(&text)
            .into_iter()
            .map(str::to_string)
            .collect();
        Ok(Self {
            text,
            lines,
            line_ending,
            final_newline,
            edits: FileEdits::default(),
        })
    }

    /// A 1-indexed line number checked against the file's length.
    fn bound(&self, path: &str, line: usize) -> Result<(), Error> {
        if line == 0 || line > self.lines.len() {
            return Err(Error::Anchoring {
                path: PathBuf::from(path),
                detail: format!("line {line} is past the file's {} lines", self.lines.len()),
            });
        }
        Ok(())
    }

    /// A range checked against the file's length.
    fn bound_range(&self, path: &str, range: Range) -> Result<(), Error> {
        self.bound(path, range.end)
    }

    /// The post-edit text, preserving original CRLF vs LF and trailing newline.
    fn new_text(&self) -> String {
        if !self.edits.has_content_ops() {
            return self.text.clone();
        }
        let mut out = String::with_capacity(self.text.len());
        for (index, line) in self.lines.iter().enumerate() {
            let line_no = index + 1;
            if let Some(lines) = self.edits.inserts.get(&index) {
                for inserted in lines {
                    out.push_str(inserted);
                    out.push_str(self.line_ending);
                }
            }
            if !self.edits.deleted.contains(&line_no) {
                out.push_str(line);
                out.push_str(self.line_ending);
            }
        }
        if let Some(lines) = self.edits.inserts.get(&self.lines.len()) {
            for inserted in lines {
                out.push_str(inserted);
                out.push_str(self.line_ending);
            }
        }
        if !self.final_newline && out.ends_with(self.line_ending) {
            out.truncate(out.len() - self.line_ending.len());
        }
        out
    }

    /// Computes individual 1-indexed inclusive new-numbering ranges for each
    /// distinct edited hunk directly from the compiled edit map.
    fn changed_hunks(&self, new_text: &str) -> Vec<Range> {
        if new_text == self.text {
            return Vec::new();
        }
        let new_lines = split_display_lines(new_text);
        let total_new = new_lines.len();
        if total_new == 0 {
            return Vec::new();
        }
        let old_lines = &self.lines;
        let old_len = old_lines.len();
        let mut hunks = Vec::new();
        let mut in_change = false;
        let mut old_start = 0usize;
        let mut new_start = 0usize;
        let mut emitted_new = 0usize;

        let flush = |hunks: &mut Vec<Range>,
                     old_start: usize,
                     old_end: usize,
                     new_start: usize,
                     new_end: usize| {
            let mut os = old_start;
            let mut oe = old_end;
            let mut ns = new_start;
            let mut ne = new_end;
            while os < oe && ns < ne && old_lines[os] == new_lines[ns] {
                os += 1;
                ns += 1;
            }
            while os < oe && ns < ne && old_lines[oe - 1] == new_lines[ne - 1] {
                oe -= 1;
                ne -= 1;
            }
            if os == oe && ns == ne {
                return;
            }
            if ns < ne {
                hunks.push(Range {
                    start: ns + 1,
                    end: ne,
                });
            } else {
                hunks.push(delete_anchor(ns, total_new));
            }
        };

        for index in 0..old_len {
            let line_no = index + 1;
            if let Some(inserted) = self.edits.inserts.get(&index)
                && !inserted.is_empty()
            {
                if !in_change {
                    in_change = true;
                    old_start = index;
                    new_start = emitted_new;
                }
                emitted_new += inserted.len();
            }
            if self.edits.deleted.contains(&line_no) {
                if !in_change {
                    in_change = true;
                    old_start = index;
                    new_start = emitted_new;
                }
            } else {
                if in_change {
                    flush(&mut hunks, old_start, index, new_start, emitted_new);
                    in_change = false;
                }
                emitted_new += 1;
            }
        }
        if let Some(inserted) = self.edits.inserts.get(&old_len)
            && !inserted.is_empty()
        {
            if !in_change {
                in_change = true;
                old_start = old_len;
                new_start = emitted_new;
            }
            emitted_new += inserted.len();
        }
        if in_change {
            flush(&mut hunks, old_start, old_len, new_start, emitted_new);
        }
        hunks
    }
}

fn delete_anchor(new_index: usize, total_new: usize) -> Range {
    if new_index == 0 {
        Range { start: 1, end: 1 }
    } else if new_index >= total_new {
        Range {
            start: total_new,
            end: total_new,
        }
    } else {
        Range {
            start: new_index,
            end: new_index + 1,
        }
    }
}

/// Parses `patch` and applies it atomically to files under `base`.
///
/// Relative section paths resolve against `base`; absolute paths and `..`
/// segments are refused by the parser. Every `LineAnchor` across every section
/// is validated against live disk contents before any write lands.
///
/// # Errors
/// Returns [`Error`] when the patch does not parse, a file is missing or
/// unreadable, any line anchor mismatches, ops overlap, a register is empty,
/// a block anchor resolves nothing, or a write fails.
pub fn apply_patch(
    base: &Path,
    patch: &str,
    resolver: &dyn BlockResolver,
) -> Result<ApplyReport, Error> {
    let sections = parse::parse(patch)?;

    // Phase 1: load every unique target file once (merging duplicate sections).
    let mut path_index: HashMap<String, usize> = HashMap::new();
    let mut files: Vec<(String, FileState)> = Vec::new();
    let mut raw_sections: Vec<(usize, Section)> = Vec::with_capacity(sections.len());

    for section in sections {
        let abs = base.join(&section.path);
        if !abs.is_file() {
            if let Some((stripped, tag)) = section.path.rsplit_once('#')
                && !stripped.is_empty()
                && tag.len() == 4
                && tag.bytes().all(|b| b.is_ascii_hexdigit())
                && base.join(stripped).is_file()
            {
                return Err(Error::File {
                    path: PathBuf::from(&section.path),
                    detail: format!(
                        "no such file — section headers use `[{stripped}]` without a `#TAG` suffix, and lines are anchored individually with `N#HH` from `search` (e.g. `PUT 12#VK:`)"
                    ),
                });
            }
            return Err(Error::File {
                path: PathBuf::from(&section.path),
                detail: "no such file — `edit` edits existing files".to_string(),
            });
        }
        let idx = if let Some(&existing) = path_index.get(&section.path) {
            existing
        } else {
            let loaded = FileState::load(&abs, &section.path)?;
            let idx = files.len();
            path_index.insert(section.path.clone(), idx);
            files.push((section.path.clone(), loaded));
            idx
        };
        raw_sections.push((idx, section));
    }

    // Phase 2: strictly validate ALL line anchors in ALL sections per file
    // before compiling or writing anything.
    let mut anchors_per_file: Vec<Vec<LineAnchor>> = vec![Vec::new(); files.len()];
    for (file_idx, section) in &raw_sections {
        for op in &section.ops {
            collect_op_anchors(op, &mut anchors_per_file[*file_idx]);
        }
    }
    for ((path, file_state), anchors) in files.iter().zip(anchors_per_file.iter()) {
        validate_file_anchors(path, file_state, anchors)?;
    }

    // Phase 3: compile ops in exact patch order so registers flow across sections.
    let mut clipboard = Clipboard::default();
    for (file_idx, section) in &raw_sections {
        let (path, file_state) = &mut files[*file_idx];
        let path_owned = path.clone();
        for op in &section.ops {
            compile_op(op, &path_owned, file_state, &mut clipboard, resolver)?;
        }
    }

    // Phase 4: write each unique file once and build the report.
    let mut report = ApplyReport {
        sections: Vec::with_capacity(files.len()),
    };
    for (path, file_state) in files {
        let section_report = write_section(base, &path, &file_state)?;
        report.sections.push(section_report);
    }
    Ok(report)
}

/// Appends every [`LineAnchor`] carried by `op` in appearance order.
fn collect_op_anchors(op: &Op, out: &mut Vec<LineAnchor>) {
    match op {
        Op::Replace { range, .. } | Op::Cut { range, .. } | Op::Yank { range, .. } => {
            out.push(range.start);
            if range.end != range.start {
                out.push(range.end);
            }
        }
        Op::ReplaceBlock { anchor, .. }
        | Op::InsertBefore { anchor, .. }
        | Op::InsertAfter { anchor, .. }
        | Op::InsertAfterBlock { anchor, .. }
        | Op::CutBlock { anchor, .. }
        | Op::YankBlock { anchor, .. } => out.push(*anchor),
        Op::Paste { target, .. } => match *target {
            PasteTarget::Before(anchor)
            | PasteTarget::After(anchor)
            | PasteTarget::AfterBlock(anchor)
            | PasteTarget::Block(anchor) => out.push(anchor),
            PasteTarget::Range(range) => {
                out.push(range.start);
                if range.end != range.start {
                    out.push(range.end);
                }
            }
            PasteTarget::BeforeStart | PasteTarget::Eof => {}
        },
        Op::InsertBeforeStart { .. } | Op::InsertEof { .. } | Op::Remove | Op::Move { .. } => {}
    }
}

/// Validates every anchor for one file, collecting ALL mismatches and moved-line
/// hints before returning an actionable [`Error::HashMismatch`].
fn validate_file_anchors(
    path: &str,
    file_state: &FileState,
    anchors: &[LineAnchor],
) -> Result<(), Error> {
    if anchors.is_empty() {
        return Ok(());
    }
    let live_hashes: Vec<[u8; 2]> = file_state.lines.iter().map(|l| line_hash(l)).collect();
    let total = file_state.lines.len();

    let mut seen_mismatches: Vec<LineAnchor> = Vec::new();
    let mut details: Vec<String> = Vec::new();
    let mut context_lines: BTreeSet<usize> = BTreeSet::new();

    for &anchor in anchors {
        if anchor.line >= 1 && anchor.line <= total && live_hashes[anchor.line - 1] == anchor.hash {
            continue;
        }
        if seen_mismatches.contains(&anchor) {
            continue;
        }
        seen_mismatches.push(anchor);

        let expected_hh = format_hash(anchor.hash);
        let mut line_msg = if anchor.line >= 1 && anchor.line <= total {
            let actual_hh = format_hash(live_hashes[anchor.line - 1]);
            format!(
                "  expected {}#{expected_hh}, found {}#{actual_hh}",
                anchor.line, anchor.line
            )
        } else {
            format!(
                "  expected {}#{expected_hh}, but file only has {total} lines",
                anchor.line
            )
        };

        // Scan all live lines for moved-line hints.
        let mut matches: Vec<usize> = live_hashes
            .iter()
            .enumerate()
            .filter_map(|(idx, &h)| (h == anchor.hash).then_some(idx + 1))
            .collect();
        let hinted_line = match matches.len() {
            1 => {
                let moved_line = matches[0];
                let _ = write!(line_msg, " (hint: #{expected_hh} is at line {moved_line})");
                Some(moved_line)
            }
            2..=3 => {
                matches.sort_unstable_by_key(|&m| (m.abs_diff(anchor.line), m));
                let list = matches
                    .iter()
                    .map(usize::to_string)
                    .collect::<Vec<_>>()
                    .join(", ");
                let _ = write!(line_msg, " (hint: #{expected_hh} matches lines {list})");
                Some(matches[0])
            }
            _ => None,
        };
        details.push(line_msg);

        if total > 0 {
            let center = anchor.line.clamp(1, total);
            let lo = center.saturating_sub(MISMATCH_CONTEXT).max(1);
            let hi = center.saturating_add(MISMATCH_CONTEXT).min(total);
            context_lines.extend(lo..=hi);
            if let Some(moved) = hinted_line {
                let lo = moved.saturating_sub(MOVED_HINT_CONTEXT).max(1);
                let hi = moved.saturating_add(MOVED_HINT_CONTEXT).min(total);
                context_lines.extend(lo..=hi);
            }
        }
    }

    if details.is_empty() {
        return Ok(());
    }

    let mut context = String::new();
    let _ = writeln!(context, "{}", section_header(path));
    if total == 0 {
        let _ = write!(context, "(empty file)");
    } else {
        let mut prev = 0usize;
        let mut emitted = 0usize;
        let mut omitted = 0usize;
        for line_no in context_lines {
            if emitted >= MAX_MISMATCH_CONTEXT_LINES {
                omitted += 1;
                continue;
            }
            if prev != 0 && line_no > prev + 1 {
                let _ = writeln!(context, "--");
            }
            let _ = writeln!(
                context,
                "{}",
                hashed_line(line_no, &file_state.lines[line_no - 1])
            );
            prev = line_no;
            emitted += 1;
        }
        if omitted > 0 {
            let _ = writeln!(context, "... ({omitted} lines omitted)");
        }
        while context.ends_with('\n') {
            context.pop();
        }
    }

    Err(Error::HashMismatch {
        path: PathBuf::from(path),
        details: details.join("\n"),
        context,
    })
}

/// Compiles one op into `state`'s edit set and updates registers.
fn compile_op(
    op: &Op,
    path: &str,
    state: &mut FileState,
    clipboard: &mut Clipboard,
    resolver: &dyn BlockResolver,
) -> Result<(), Error> {
    match op {
        Op::Replace { range, body } => {
            let lines = range.lines();
            state.bound_range(path, lines)?;
            state.edits.insert(lines.start - 1, body.iter().cloned());
            state.edits.delete(path, lines)?;
        }
        Op::ReplaceBlock { anchor, body } => {
            let span = block_at(path, state, anchor.line, resolver)?;
            state.edits.insert(span.start - 1, body.iter().cloned());
            state.edits.delete(path, span)?;
        }
        Op::InsertBeforeStart { body } => {
            state.edits.insert(0, body.iter().cloned());
        }
        Op::InsertBefore { anchor, body } => {
            state.bound(path, anchor.line)?;
            state.edits.insert(anchor.line - 1, body.iter().cloned());
        }
        Op::InsertAfter { anchor, body } => {
            state.bound(path, anchor.line)?;
            state.edits.insert(anchor.line, body.iter().cloned());
        }
        Op::InsertEof { body } => {
            state.edits.insert(state.lines.len(), body.iter().cloned());
        }
        Op::InsertAfterBlock { anchor, body } => {
            let span = block_at(path, state, anchor.line, resolver)?;
            state.edits.insert(span.end, body.iter().cloned());
        }
        Op::Cut { range, register } => {
            let lines = range.lines();
            state.bound_range(path, lines)?;
            let captured: Vec<String> = state.lines[lines.start - 1..lines.end].to_vec();
            clipboard.capture(register.as_deref(), captured);
            state.edits.delete(path, lines)?;
        }
        Op::CutBlock { anchor, register } => {
            let span = block_at(path, state, anchor.line, resolver)?;
            let captured: Vec<String> = state.lines[span.start - 1..span.end].to_vec();
            clipboard.capture(register.as_deref(), captured);
            state.edits.delete(path, span)?;
        }
        Op::Yank { register, range } => {
            let lines = range.lines();
            state.bound_range(path, lines)?;
            let captured: Vec<String> = state.lines[lines.start - 1..lines.end].to_vec();
            clipboard.capture(Some(register.as_str()), captured);
        }
        Op::YankBlock { register, anchor } => {
            let span = block_at(path, state, anchor.line, resolver)?;
            let captured: Vec<String> = state.lines[span.start - 1..span.end].to_vec();
            clipboard.capture(Some(register.as_str()), captured);
        }
        Op::Paste { target, register } => {
            let content = clipboard.paste(path, register.as_deref())?;
            let (gap, range) = match *target {
                PasteTarget::BeforeStart => (0, None),
                PasteTarget::Before(anchor) => {
                    state.bound(path, anchor.line)?;
                    (anchor.line - 1, None)
                }
                PasteTarget::After(anchor) => {
                    state.bound(path, anchor.line)?;
                    (anchor.line, None)
                }
                PasteTarget::Eof => (state.lines.len(), None),
                PasteTarget::AfterBlock(anchor) => {
                    (block_at(path, state, anchor.line, resolver)?.end, None)
                }
                PasteTarget::Range(range) => {
                    let lines = range.lines();
                    state.bound_range(path, lines)?;
                    (lines.start - 1, Some(lines))
                }
                PasteTarget::Block(anchor) => {
                    let span = block_at(path, state, anchor.line, resolver)?;
                    (span.start - 1, Some(span))
                }
            };
            state.edits.insert(gap, content);
            if let Some(range) = range {
                state.edits.delete(path, range)?;
            }
        }
        Op::Remove => {
            if state.edits.has_content_ops() || state.edits.move_to.is_some() {
                return Err(Error::Anchoring {
                    path: PathBuf::from(path),
                    detail: "`REM` stands alone in its section — remove the other ops".to_string(),
                });
            }
            state.edits.remove = true;
        }
        Op::Move { dest } => {
            if state.edits.remove {
                return Err(Error::Anchoring {
                    path: PathBuf::from(path),
                    detail: "a section cannot both `MV` and `REM`".to_string(),
                });
            }
            if state.edits.move_to.is_some() {
                return Err(Error::Anchoring {
                    path: PathBuf::from(path),
                    detail: "only one `MV` per section".to_string(),
                });
            }
            if Path::new(dest).is_absolute() || dest.split('/').any(|s| s == "..") {
                return Err(Error::Anchoring {
                    path: PathBuf::from(path),
                    detail: "`MV` destinations are relative, without `..` segments".to_string(),
                });
            }
            state.edits.move_to = Some(dest.clone());
        }
    }
    Ok(())
}

/// Resolves the block beginning at a 1-indexed line, or [`Error::NoBlock`].
fn block_at(
    path: &str,
    state: &FileState,
    line: usize,
    resolver: &dyn BlockResolver,
) -> Result<Range, Error> {
    state.bound(path, line)?;
    resolver
        .block_span(Path::new(path), &state.text, line)
        .map(|(start, end)| Range { start, end })
        .ok_or(Error::NoBlock {
            path: PathBuf::from(path),
            line,
            hint: "",
        })
}

/// Writes one file's post-edit state and builds its report.
fn write_section(base: &Path, path: &str, state: &FileState) -> Result<SectionReport, Error> {
    let abs = base.join(path);
    if state.edits.remove {
        std::fs::remove_file(&abs).map_err(|source| Error::Io {
            path: abs.clone(),
            source,
        })?;
        return Ok(SectionReport {
            path: PathBuf::from(path),
            outcome: FileOutcome::Removed,
            changed: None,
            hunks: Vec::new(),
            line_delta: -(state.lines.len() as isize),
        });
    }
    let new_text = state.new_text();
    let target = state
        .edits
        .move_to
        .as_deref()
        .map_or_else(|| abs.clone(), |dest| base.join(dest));
    let metadata = std::fs::metadata(&abs).ok();
    write_atomic(&abs, &target, new_text.as_bytes(), metadata.as_ref())?;
    if state.edits.move_to.is_some() {
        std::fs::remove_file(&abs).map_err(|source| Error::Io {
            path: abs.clone(),
            source,
        })?;
    }

    let hunks = state.changed_hunks(&new_text);
    let changed = (!hunks.is_empty()).then(|| Range {
        start: hunks[0].start,
        end: hunks[hunks.len() - 1].end,
    });
    let actually_changed = new_text != state.text;
    let old_count = state.lines.len() as isize;
    let new_count = split_display_lines(&new_text).len() as isize;
    let line_delta = new_count - old_count;

    let outcome = match (&state.edits.move_to, actually_changed) {
        (Some(_), _) => FileOutcome::Moved {
            dest: PathBuf::from(state.edits.move_to.clone().expect("checked")),
        },
        (None, true) => FileOutcome::Modified,
        (None, false) => FileOutcome::Unchanged,
    };
    Ok(SectionReport {
        path: PathBuf::from(path),
        outcome,
        changed,
        hunks,
        line_delta,
    })
}

/// Atomically writes `content` to `target`, preserving `source`'s mode when
/// the destination is new.
fn write_atomic(
    source: &Path,
    target: &Path,
    content: &[u8],
    metadata: Option<&std::fs::Metadata>,
) -> Result<(), Error> {
    let parent = target.parent().ok_or_else(|| Error::File {
        path: target.to_path_buf(),
        detail: "has no parent directory".to_string(),
    })?;
    // Skip `create_dir_all` when `parent` already exists: under Linux Landlock,
    // `mkdir(parent)` checks `MAKE_DIR` on `parent`'s parent before returning
    // `EEXIST`, failing with `EACCES` at the root of an approved workspace.
    if !parent.is_dir() {
        std::fs::create_dir_all(parent).map_err(|source_err| Error::Io {
            path: parent.to_path_buf(),
            source: source_err,
        })?;
    }
    let tmp = parent.join(format!(
        ".{}.hashline-tmp",
        target
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    ));
    std::fs::write(&tmp, content).map_err(|source_err| Error::Io {
        path: tmp.clone(),
        source: source_err,
    })?;
    if let Some(meta) = metadata {
        let _ = std::fs::set_permissions(&tmp, meta.permissions());
    }
    std::fs::rename(&tmp, target).map_err(|source_err| Error::Io {
        path: target.to_path_buf(),
        source: source_err,
    })?;
    let _ = source;
    Ok(())
}

/// Formats per-hunk `±2` hashed context windows (`N#HH|content`) for `hunks`,
/// merging overlapping/adjacent windows, separating disjoint windows with `--`,
/// and eliding any single window > 40 lines with head(25) + `... (N lines omitted)`
/// + tail(10) without hiding subsequent hunks.
#[must_use]
pub fn format_hunk_echo(text: &str, hunks: &[Range]) -> String {
    let lines = split_display_lines(text);
    if lines.is_empty() || hunks.is_empty() {
        return String::new();
    }
    let total = lines.len();
    let mut windows: Vec<(usize, usize)> = hunks
        .iter()
        .filter_map(|range| {
            let start = range.start.saturating_sub(ECHO_CONTEXT).max(1).min(total);
            let end = range.end.saturating_add(ECHO_CONTEXT).min(total);
            (start <= end).then_some((start, end))
        })
        .collect();
    if windows.is_empty() {
        return String::new();
    }
    windows.sort_unstable_by_key(|&(start, _)| start);
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(windows.len());
    for (start, end) in windows {
        match merged.last_mut() {
            Some((_, prev_end)) if start <= *prev_end + 1 => {
                *prev_end = (*prev_end).max(end);
            }
            _ => merged.push((start, end)),
        }
    }

    let mut out = String::new();
    for (win_idx, (start, end)) in merged.into_iter().enumerate() {
        if win_idx > 0 {
            let _ = writeln!(out, "--");
        }
        let count = end - start + 1;
        if count <= ECHO_LIMIT {
            for line_no in start..=end {
                let _ = writeln!(out, "{}", hashed_line(line_no, lines[line_no - 1]));
            }
        } else {
            for line_no in start..start + ECHO_HEAD {
                let _ = writeln!(out, "{}", hashed_line(line_no, lines[line_no - 1]));
            }
            let omitted = count - ECHO_HEAD - ECHO_TAIL;
            let _ = writeln!(out, "... ({omitted} lines omitted)");
            let tail_start = end - ECHO_TAIL + 1;
            for line_no in tail_start..=end {
                let _ = writeln!(out, "{}", hashed_line(line_no, lines[line_no - 1]));
            }
        }
    }
    out
}

/// The `[path]` header a report's file gets in the edit response, annotated
/// with `(+N lines)` / `(-N lines)` when the line count changed.
#[must_use]
pub fn report_header(report: &SectionReport) -> String {
    let display = match &report.outcome {
        FileOutcome::Moved { dest } => dest.to_string_lossy(),
        _ => report.path.to_string_lossy(),
    };
    let base = section_header(&display);
    if matches!(report.outcome, FileOutcome::Removed) {
        return format!("{base} (removed)");
    }
    if report.line_delta == 0 {
        return base;
    }
    let abs_delta = report.line_delta.unsigned_abs();
    let unit = if abs_delta == 1 { "line" } else { "lines" };
    if report.line_delta > 0 {
        format!("{base} (+{abs_delta} {unit})")
    } else {
        format!("{base} (-{abs_delta} {unit})")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_hunk_echo_elides_giant_hunk_with_25_and_10_without_hiding_next_hunk() {
        let mut lines = Vec::new();
        for i in 1..=140 {
            lines.push(format!("line_{i}"));
        }
        let text = format!("{}\n", lines.join("\n"));

        // Giant hunk spanning 10..=70 (expanded 8..=72 = 65 lines > 40) plus disjoint hunk at 130.
        let echo = format_hunk_echo(
            &text,
            &[
                Range { start: 10, end: 70 },
                Range {
                    start: 130,
                    end: 130,
                },
            ],
        );

        // Head(25): 8..=32, omitted: 65 - 25 - 10 = 30, tail(10): 63..=72.
        assert!(
            echo.contains(&format!("{}\n", hashed_line(8, "line_8"))),
            "echo:\n{echo}"
        );
        assert!(
            echo.contains(&format!("{}\n", hashed_line(32, "line_32"))),
            "echo:\n{echo}"
        );
        assert!(
            !echo.contains(&format!("{}\n", hashed_line(33, "line_33"))),
            "echo:\n{echo}"
        );
        assert!(echo.contains("... (30 lines omitted)\n"), "echo:\n{echo}");
        assert!(
            !echo.contains(&format!("{}\n", hashed_line(62, "line_62"))),
            "echo:\n{echo}"
        );
        assert!(
            echo.contains(&format!("{}\n", hashed_line(63, "line_63"))),
            "echo:\n{echo}"
        );
        assert!(
            echo.contains(&format!("{}\n", hashed_line(72, "line_72"))),
            "echo:\n{echo}"
        );
        // Separate disjoint hunk at 130 (expanded 128..=132) is NEVER hidden!
        assert!(
            echo.contains(&format!("\n--\n{}\n", hashed_line(128, "line_128"))),
            "echo:\n{echo}"
        );
        assert!(
            echo.contains(&format!("{}\n", hashed_line(132, "line_132"))),
            "echo:\n{echo}"
        );
    }

    #[test]
    fn report_header_formats_positive_negative_zero_and_removed() {
        let base = SectionReport {
            path: PathBuf::from("a.rs"),
            outcome: FileOutcome::Modified,
            changed: None,
            hunks: Vec::new(),
            line_delta: 0,
        };
        assert_eq!(report_header(&base), "[a.rs]");

        let added = SectionReport {
            line_delta: 11,
            ..base
        };
        assert_eq!(report_header(&added), "[a.rs] (+11 lines)");

        let removed = SectionReport {
            line_delta: -3,
            ..added
        };
        assert_eq!(report_header(&removed), "[a.rs] (-3 lines)");

        let one = SectionReport {
            line_delta: 1,
            ..removed
        };
        assert_eq!(report_header(&one), "[a.rs] (+1 line)");

        let deleted_file = SectionReport {
            outcome: FileOutcome::Removed,
            line_delta: -10,
            ..one
        };
        assert_eq!(report_header(&deleted_file), "[a.rs] (removed)");
    }
}
