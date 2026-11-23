//! Parser for the hashline patch language an `edit` call reads on stdin.
//!
//! A patch is a list of `[path#TAG]` sections. Inside a section each line is
//! an op header; `+` rows under a `PUT …:` header are that op's literal body.
//! The grammar:
//!
//! ```text
//! [src/main.rs#1A2B]
//! PUT 5.=9:        replace original lines 5 through 9 with the + rows below
//! +    let x = 1;
//! PUT <3:          insert + rows before line 3
//! PUT >4:          insert + rows after line 4  (PUT >$: appends at end)
//! PUT >5*:         insert + rows after the block beginning on line 5
//! PUT 6*:          replace the block beginning on line 6
//! PUT <7 @reg      paste register @reg before line 7 (no colon, no body)
//! PUT 8.=10 @reg   paste register @reg over lines 8–10
//! CUT 11.=13 [@reg]  delete lines and capture them into a register
//! CUT 14*  [@reg]    delete the block beginning on line 14, captured
//! REM              delete the file
//! MV new/path.rs   write the edited content at DEST and remove the file
//! ```

use crate::error::Error;

/// A 1-indexed inclusive line range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Range {
    /// First line covered.
    pub start: usize,
    /// Last line covered.
    pub end: usize,
}

/// Where a register paste lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasteTarget {
    /// The gap before a line.
    Before(usize),
    /// The gap after a line.
    After(usize),
    /// End of file.
    Eof,
    /// The gap after the block beginning at a line.
    AfterBlock(usize),
    /// Over an inclusive line range; requires a named register.
    Range(Range),
    /// Over the block beginning at a line; requires a named register.
    Block(usize),
}

/// One operation inside a section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// `PUT N.=M:` — replace the inclusive range with the body rows.
    Replace { range: Range, body: Vec<String> },
    /// `PUT N*:` — replace the block beginning on the line.
    ReplaceBlock { line: usize, body: Vec<String> },
    /// `PUT <N:` — insert the body rows before the line.
    InsertBefore { line: usize, body: Vec<String> },
    /// `PUT >N:` — insert the body rows after the line.
    InsertAfter { line: usize, body: Vec<String> },
    /// `PUT >$:` — insert the body rows at end of file.
    InsertEof { body: Vec<String> },
    /// `PUT >N*:` — insert the body rows after the block at the line.
    InsertAfterBlock { line: usize, body: Vec<String> },
    /// `CUT N.=M [@reg]` — delete the range and capture it.
    Cut {
        range: Range,
        register: Option<String>,
    },
    /// `CUT N* [@reg]` — delete the block at the line and capture it.
    CutBlock {
        line: usize,
        register: Option<String>,
    },
    /// `PUT … @reg` — paste a register at a gap or over a span.
    Paste {
        target: PasteTarget,
        register: Option<String>,
    },
    /// `REM` — delete the file.
    Remove,
    /// `MV DEST` — write the result at DEST and remove the source.
    Move { dest: String },
}

/// One `[path#TAG]` section: a file and the ops that address its lines.
#[derive(Debug, Clone)]
pub struct Section {
    /// The path as the header named it, relative to the working directory.
    pub path: String,
    /// The 4-hex tag the header carried.
    pub tag: String,
    /// The ops in patch order.
    pub ops: Vec<Op>,
}

/// The parsing state while a `PUT …:` header collects `+` body rows.
struct PendingBody {
    target: PutTarget,
    body: Vec<String>,
}

/// The span a `PUT` addresses, before body-vs-register interpretation.
enum PutTarget {
    Range(Range),
    Block(usize),
    Before(usize),
    After(usize),
    Eof,
    AfterBlock(usize),
}

impl PasteTarget {
    /// The paste form of a `PutTarget`, with the register rules applied.
    fn of(
        target: &PutTarget,
        register: Option<String>,
        path: &str,
        line: usize,
    ) -> Result<Self, Error> {
        let named = register.is_some();
        Ok(match *target {
            PutTarget::Before(line) => Self::Before(line),
            PutTarget::After(line) => Self::After(line),
            PutTarget::Eof => Self::Eof,
            PutTarget::AfterBlock(line) => Self::AfterBlock(line),
            PutTarget::Range(range) if named => Self::Range(range),
            PutTarget::Block(line) if named => Self::Block(line),
            PutTarget::Range(_) | PutTarget::Block(_) => {
                return Err(Error::Parse {
                    line,
                    detail: format!(
                        "{path}: a span or block paste needs a named register — `PUT … @name`"
                    ),
                });
            }
        })
    }
}

/// Parses `patch` into its sections.
///
/// # Errors
/// Returns [`Error::Parse`] with the 1-indexed input line on malformed input.
pub fn parse(patch: &str) -> Result<Vec<Section>, Error> {
    let mut sections: Vec<Section> = Vec::new();
    let mut pending: Option<PendingBody> = None;
    for (index, raw) in patch.lines().enumerate() {
        let line_no = index + 1;
        if raw.trim().is_empty() {
            continue;
        }
        // A `+` row only lives under a pending PUT header.
        if let Some(rest) = raw.strip_prefix('+') {
            match pending.as_mut() {
                Some(body) => body.body.push(rest.to_string()),
                None => {
                    return Err(Error::Parse {
                        line: line_no,
                        detail: "a `+` body row needs a `PUT …:` header above it".to_string(),
                    });
                }
            }
            continue;
        }
        if let Some(pending) = pending.take() {
            commit_pending(pending, sections.last_mut(), line_no)?;
        }
        if let Some(section) = parse_header(raw, line_no)? {
            sections.push(section);
            continue;
        }
        let section = sections.last_mut().ok_or(Error::Parse {
            line: line_no,
            detail: "an op needs a `[path#TAG]` section header above it".to_string(),
        })?;
        parse_op(raw, section, line_no, &mut pending)?;
    }
    if let Some(pending) = pending.take() {
        commit_pending(pending, sections.last_mut(), patch.lines().count())?;
    }
    if sections.is_empty() {
        return Err(Error::Parse {
            line: 0,
            detail: "the patch holds no `[path#TAG]` section".to_string(),
        });
    }
    Ok(sections)
}

/// Turns a pending `PUT …:` header and its collected body into an op.
fn commit_pending(
    pending: PendingBody,
    section: Option<&mut Section>,
    line: usize,
) -> Result<(), Error> {
    if pending.body.is_empty() {
        return Err(Error::Parse {
            line,
            detail: "a `PUT` header needs at least one `+` body row (`CUT` deletes, a ".to_string()
                + "register paste takes no body)",
        });
    }
    let op = match pending.target {
        PutTarget::Range(range) => Op::Replace {
            range,
            body: pending.body,
        },
        PutTarget::Block(line) => Op::ReplaceBlock {
            line,
            body: pending.body,
        },
        PutTarget::Before(line) => Op::InsertBefore {
            line,
            body: pending.body,
        },
        PutTarget::After(line) => Op::InsertAfter {
            line,
            body: pending.body,
        },
        PutTarget::Eof => Op::InsertEof { body: pending.body },
        PutTarget::AfterBlock(line) => Op::InsertAfterBlock {
            line,
            body: pending.body,
        },
    };
    section
        .expect("a pending PUT header only follows a section")
        .ops
        .push(op);
    Ok(())
}

/// Parses a `[path#TAG]` header line, or `None` when it is not one.
fn parse_header(raw: &str, line: usize) -> Result<Option<Section>, Error> {
    let inner = raw
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'));
    let Some(inner) = inner else {
        return Ok(None);
    };
    let Some((path, tag)) = inner.rsplit_once('#') else {
        return Ok(None);
    };
    if tag.len() != 4 || !tag.chars().all(|c| c.is_ascii_hexdigit()) {
        return Ok(None);
    }
    if path.is_empty() || path.starts_with('/') || path.split('/').any(|segment| segment == "..") {
        return Err(Error::Parse {
            line,
            detail: "section paths are relative, without `..` segments".to_string(),
        });
    }
    Ok(Some(Section {
        path: path.to_string(),
        tag: tag.to_uppercase(),
        ops: Vec::new(),
    }))
}

/// Parses one op line inside `section`.
fn parse_op(
    raw: &str,
    section: &mut Section,
    line: usize,
    pending: &mut Option<PendingBody>,
) -> Result<(), Error> {
    let path = section.path.clone();
    let fail = |detail: String| Error::Parse {
        line,
        detail: format!("{path}: {detail}"),
    };
    if raw == "REM" {
        section.ops.push(Op::Remove);
        return Ok(());
    }
    if let Some(dest) = raw.strip_prefix("MV ") {
        let dest = unquote(dest.trim());
        if dest.is_empty() {
            return Err(fail("`MV` needs a destination".to_string()));
        }
        section.ops.push(Op::Move { dest });
        return Ok(());
    }
    if let Some(rest) = raw.strip_prefix("CUT ") {
        let (target, register) = split_register(rest.trim());
        match parse_span(target, line, &path)? {
            CutSpan::Range(range) => section.ops.push(Op::Cut { range, register }),
            CutSpan::Block(n) => section.ops.push(Op::CutBlock { line: n, register }),
        }
        return Ok(());
    }
    if let Some(rest) = raw.strip_prefix("PUT ") {
        let rest = rest.trim();
        if let Some(target) = rest.strip_suffix(':') {
            *pending = Some(PendingBody {
                target: parse_put_target(target.trim(), line, &path)?,
                body: Vec::new(),
            });
            return Ok(());
        }
        let (target, register) = split_register(rest);
        let put_target = parse_put_target(target, line, &path)?;
        section.ops.push(Op::Paste {
            target: PasteTarget::of(&put_target, register.clone(), &path, line)?,
            register,
        });
        return Ok(());
    }
    Err(fail(format!("unrecognized op `{raw}`")))
}

/// A `CUT` span: a range or a block anchor.
enum CutSpan {
    Range(Range),
    Block(usize),
}

/// Parses `N`, `N.=M`, or `N*` for `CUT`.
fn parse_span(raw: &str, line: usize, path: &str) -> Result<CutSpan, Error> {
    let fail = || Error::Parse {
        line,
        detail: format!("{path}: expected `CUT N.=M` or `CUT N*`, got `CUT {raw}`"),
    };
    if let Some(n) = raw.strip_suffix('*') {
        return Ok(CutSpan::Block(parse_line(n, line, path)?));
    }
    Ok(CutSpan::Range(
        parse_range(raw, line, path).map_err(|_| fail())?,
    ))
}

/// Splits a trailing `@name` register off an op argument.
fn split_register(raw: &str) -> (&str, Option<String>) {
    match raw.split_once('@') {
        Some((target, name)) if !target.trim().is_empty() && !name.trim().is_empty() => {
            (target.trim(), Some(name.trim().to_string()))
        }
        _ => (raw.trim(), None),
    }
}

/// Parses the part of a `PUT` line before `:`/`@`.
fn parse_put_target(raw: &str, line: usize, path: &str) -> Result<PutTarget, Error> {
    let fail = || Error::Parse {
        line,
        detail: format!(
            "{path}: expected `PUT N.=M`, `PUT <N`, `PUT >N`, `PUT >$`, `PUT N*` or `PUT >N*`, got `PUT {raw}`"
        ),
    };
    if let Some(n) = raw.strip_prefix('<') {
        return parse_line(n, line, path).map(PutTarget::Before);
    }
    if let Some(rest) = raw.strip_prefix('>') {
        if rest == "$" {
            return Ok(PutTarget::Eof);
        }
        if let Some(n) = rest.strip_suffix('*') {
            return parse_line(n, line, path).map(PutTarget::AfterBlock);
        }
        return parse_line(rest, line, path).map(PutTarget::After);
    }
    if let Some(n) = raw.strip_suffix('*') {
        return parse_line(n, line, path).map(PutTarget::Block);
    }
    parse_range(raw, line, path)
        .map(PutTarget::Range)
        .map_err(|_| fail())
}

/// Parses `N` or `N.=M` as an inclusive range.
fn parse_range(raw: &str, line: usize, path: &str) -> Result<Range, Error> {
    let fail = || Error::Parse {
        line,
        detail: format!("{path}: expected a `N.=M` range, got `{raw}`"),
    };
    let (start, end) = match raw.split_once(".=") {
        Some((a, b)) => (a, b),
        None => (raw, raw),
    };
    let range = Range {
        start: parse_line(start, line, path).map_err(|_| fail())?,
        end: parse_line(end, line, path).map_err(|_| fail())?,
    };
    if range.start > range.end {
        return Err(fail());
    }
    Ok(range)
}

/// Parses a positive line number.
fn parse_line(raw: &str, line: usize, path: &str) -> Result<usize, Error> {
    raw.trim()
        .parse::<usize>()
        .ok()
        .filter(|n| *n > 0)
        .ok_or(Error::Parse {
            line,
            detail: format!("{path}: `{raw}` is not a line number"),
        })
}

/// Removes one level of `"…"` quoting from a path argument.
fn unquote(raw: &str) -> String {
    match raw.strip_prefix('"').and_then(|r| r.strip_suffix('"')) {
        Some(inner) => inner.to_string(),
        None => raw.to_string(),
    }
}
