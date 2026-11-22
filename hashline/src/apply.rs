//! Applies a parsed hashline patch to files under a base directory.
//!
//! Line numbers in a patch always address the file as the tag that read
//! minted it: edits are compiled against the original lines and applied in
//! one pass, so applied hunks never shift the numbers of the ops that follow.
//! Every file's tag is verified before any write lands, so a stale file
//! refuses the whole patch rather than half-applying it.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use crate::block::BlockResolver;
use crate::error::Error;
use crate::parse::{self, Op, PasteTarget, Range, Section};
use crate::{file_tag, section_header};

/// The result of applying a whole patch.
#[derive(Debug)]
pub struct ApplyReport {
    /// One report per section the patch held, in patch order.
    pub sections: Vec<SectionReport>,
}

/// What one section's file became.
#[derive(Debug)]
pub struct SectionReport {
    /// The section's path relative to the base directory.
    pub path: PathBuf,
    /// What happened to the file.
    pub outcome: FileOutcome,
    /// The tag the new content hashes to (`None` when the file was removed).
    pub new_tag: Option<String>,
    /// The 1-indexed inclusive span of lines the edit changed in the new
    /// numbering, when the outcome was a change.
    pub changed: Option<Range>,
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
    /// Lines captured into named registers.
    named: HashMap<String, Vec<String>>,
}

impl Clipboard {
    /// Records the lines a `CUT` captured.
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

    /// Reads the register a `PUT` names.
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
                            "several unlabeled `CUT`s precede this paste — name the one it "
                                .to_string()
                                + "means: `CUT … @name` and `PUT … @name`"
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

    /// Whether an op other than `REM`/`MV` was compiled.
    fn has_content_ops(&self) -> bool {
        !self.deleted.is_empty() || !self.inserts.is_empty()
    }
}

/// One file's state while a patch is being compiled and applied.
struct FileState {
    /// Original text, whole.
    text: String,
    /// The lines `text` splits into on `\n`.
    lines: Vec<String>,
    /// Whether `text` ends in a newline.
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
        let final_newline = text.ends_with('\n');
        let lines = text.split('\n').map(str::to_string).collect::<Vec<_>>();
        let lines = if final_newline {
            lines[..lines.len() - 1].to_vec()
        } else {
            lines
        };
        Ok(Self {
            text,
            lines,
            final_newline,
            edits: FileEdits::default(),
        })
    }

    /// A 1-indexed line number checked against the file's length.
    fn bound(&self, path: &str, line: usize) -> Result<(), Error> {
        if line > self.lines.len() {
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

    /// The post-edit text.
    fn new_text(&self) -> String {
        let mut out = String::with_capacity(self.text.len());
        for (index, line) in self.lines.iter().enumerate() {
            let line_no = index + 1;
            if let Some(lines) = self.edits.inserts.get(&index) {
                for inserted in lines {
                    out.push_str(inserted);
                    out.push('\n');
                }
            }
            if !self.edits.deleted.contains(&line_no) {
                out.push_str(line);
                out.push('\n');
            }
        }
        if let Some(lines) = self.edits.inserts.get(&self.lines.len()) {
            for inserted in lines {
                out.push_str(inserted);
                out.push('\n');
            }
        }
        if !self.final_newline && out.ends_with('\n') {
            out.pop();
        }
        out
    }

    /// The 1-indexed span of new-numbering lines the edit changed, or `None`
    /// when the post-edit text equals the original.
    fn changed_span(&self, new_text: &str) -> Option<Range> {
        let new_lines: Vec<&str> = new_text.split('\n').collect();
        let old_lines: Vec<&str> = self.text.split('\n').collect();
        let common = old_lines.len().min(new_lines.len());
        let mut first = 0;
        while first < common && old_lines[first] == new_lines[first] {
            first += 1;
        }
        let mut tail = 0;
        while tail < common - first
            && old_lines[old_lines.len() - 1 - tail] == new_lines[new_lines.len() - 1 - tail]
        {
            tail += 1;
        }
        if first == old_lines.len() && first == new_lines.len() {
            return None;
        }
        let end = new_lines.len().saturating_sub(tail);
        Some(Range {
            start: first + 1,
            end: end.max(first + 1),
        })
    }
}

/// Parses `patch` and applies it to files under `base`.
///
/// Relative section paths resolve against `base`; absolute paths are refused
/// by the parser. All file tags are verified before any write lands.
///
/// # Errors
/// Returns [`Error`] when the patch does not parse, a file is missing or
/// unreadable, a tag is stale, ops misaddress lines or overlap, a register is
/// empty, a block anchor resolves nothing, or a write fails.
pub fn apply_patch(
    base: &Path,
    patch: &str,
    resolver: &dyn BlockResolver,
) -> Result<ApplyReport, Error> {
    let sections = parse::parse(patch)?;

    // Load and tag-check every file before compiling a single edit.
    let mut files: Vec<(Section, FileState)> = Vec::with_capacity(sections.len());
    for section in sections {
        let abs = base.join(&section.path);
        if !abs.is_file() {
            return Err(Error::File {
                path: PathBuf::from(&section.path),
                detail: "no such file — `edit` edits existing files".to_string(),
            });
        }
        let state = FileState::load(&abs, &section.path)?;
        let live = file_tag(&state.text);
        if live != section.tag {
            return Err(Error::StaleTag {
                path: PathBuf::from(&section.path),
                expected: section.tag,
                live,
            });
        }
        files.push((section, state));
    }

    // Compile ops file by file, in patch order, so registers flow across
    // sections the way a `CUT` in one feeds a `PUT` in the next.
    let mut clipboard = Clipboard::default();
    for (section, state) in &mut files {
        let path = section.path.clone();
        for op in &section.ops {
            compile_op(op, &path, state, &mut clipboard, resolver)?;
        }
    }

    // Apply and write each file, collecting reports.
    let mut report = ApplyReport {
        sections: Vec::with_capacity(files.len()),
    };
    for (section, state) in files {
        report
            .sections
            .push(write_section(base, &section.path, &state)?);
    }
    Ok(report)
}

/// Compiles one op into `state`'s edit set, updating registers.
fn compile_op(
    op: &Op,
    path: &str,
    state: &mut FileState,
    clipboard: &mut Clipboard,
    resolver: &dyn BlockResolver,
) -> Result<(), Error> {
    match op {
        Op::Replace { range, body } => {
            state.bound_range(path, *range)?;
            state.edits.insert(range.start - 1, body.iter().cloned());
            state.edits.delete(path, *range)?;
        }
        Op::ReplaceBlock { line, body } => {
            let span = block_at(path, state, *line, resolver)?;
            state.edits.insert(span.start - 1, body.iter().cloned());
            state.edits.delete(path, span)?;
        }
        Op::InsertBefore { line, body } => {
            state.bound(path, *line)?;
            state.edits.insert(line - 1, body.iter().cloned());
        }
        Op::InsertAfter { line, body } => {
            state.bound(path, *line)?;
            state.edits.insert(*line, body.iter().cloned());
        }
        Op::InsertEof { body } => {
            state.edits.insert(state.lines.len(), body.iter().cloned());
        }
        Op::InsertAfterBlock { line, body } => {
            let span = block_at(path, state, *line, resolver)?;
            state.edits.insert(span.end, body.iter().cloned());
        }
        Op::Cut { range, register } => {
            state.bound_range(path, *range)?;
            let captured: Vec<String> = state.lines[range.start - 1..range.end].to_vec();
            clipboard.capture(register.as_deref(), captured);
            state.edits.delete(path, *range)?;
        }
        Op::CutBlock { line, register } => {
            let span = block_at(path, state, *line, resolver)?;
            let captured: Vec<String> = state.lines[span.start - 1..span.end].to_vec();
            clipboard.capture(register.as_deref(), captured);
            state.edits.delete(path, span)?;
        }
        Op::Paste { target, register } => {
            let content = clipboard.paste(path, register.as_deref())?;
            let (gap, range) = match *target {
                PasteTarget::Before(line) => {
                    state.bound(path, line)?;
                    (line - 1, None)
                }
                PasteTarget::After(line) => {
                    state.bound(path, line)?;
                    (line, None)
                }
                PasteTarget::Eof => (state.lines.len(), None),
                PasteTarget::AfterBlock(line) => (block_at(path, state, line, resolver)?.end, None),
                PasteTarget::Range(range) => {
                    state.bound_range(path, range)?;
                    (range.start - 1, Some(range))
                }
                PasteTarget::Block(line) => {
                    let span = block_at(path, state, line, resolver)?;
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

/// Resolves the block beginning at a 1-indexed line, or a [`Error::NoBlock`].
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
            new_tag: None,
            changed: None,
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
    let changed = state.changed_span(&new_text);
    let outcome = match (&state.edits.move_to, changed.is_some()) {
        (Some(_), _) => FileOutcome::Moved {
            dest: PathBuf::from(state.edits.move_to.clone().expect("checked")),
        },
        (None, true) => FileOutcome::Modified,
        (None, false) => FileOutcome::Unchanged,
    };
    Ok(SectionReport {
        path: PathBuf::from(path),
        outcome,
        new_tag: Some(file_tag(&new_text)),
        changed,
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
    std::fs::create_dir_all(parent).map_err(|source_err| Error::Io {
        path: parent.to_path_buf(),
        source: source_err,
    })?;
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

/// The `[path#TAG]` header a report's file gets in the edit response.
#[must_use]
pub fn report_header(report: &SectionReport) -> String {
    report
        .new_tag
        .as_deref()
        .map(|tag| section_header(&report.path.to_string_lossy(), tag))
        .unwrap_or_else(|| format!("[{}#----]", report.path.to_string_lossy()))
}
