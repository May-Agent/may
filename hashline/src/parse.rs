//! Parser for the stateless per-line hashline patch language an `edit` call
//! reads on stdin.
//!
//! A patch is a list of `[path]` sections. Inside a section each line is an
//! op header; `+` rows under a `PUT …:` header are that op's literal body.
//! Every anchored line carries a 2-character content hash (`N#HH`):
//!
//! ```text
//! [src/main.rs]
//! PUT 5#PM.=9#YN:     replace original lines 5 through 9 with the + rows below
//! +    let x = 1;
//! PUT 5#PM:           replace single line 5
//! PUT <3#VK:          insert + rows before line 3  (PUT <1: = start of file)
//! PUT >4#WS:          insert + rows after line 4   (PUT >$: = end of file)
//! PUT >5#PM*:         insert + rows after the block beginning on line 5
//! PUT 6#TX*:          replace the block beginning on line 6
//! CUT 11#JB.=13#HY    delete lines (optional @reg capture)
//! CUT 14#ZP*          delete the block beginning on line 14 (optional @reg)
//! YANK r 5#PM.=9#YN   copy lines into register r without deleting
//! YANK r 6#TX*        copy block into register r without deleting
//! PASTE r <7#NK       paste register r before line 7 (also >N#HH, <1, >$, ranges, blocks)
//! REM                 delete the file
//! MV new/path.rs      write the edited content at DEST and remove the file
//! ```

use std::fmt;

use crate::LINE_HASH_ALPHABET;
use crate::error::Error;

/// A 1-indexed inclusive line span (resolved line numbers without hashes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Range {
    /// First line covered.
    pub start: usize,
    /// Last line covered.
    pub end: usize,
}

/// A 1-indexed line number paired with its 2-character content hash (`N#HH`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineAnchor {
    /// 1-indexed line number.
    pub line: usize,
    /// Two uppercase characters from [`LINE_HASH_ALPHABET`].
    pub hash: [u8; 2],
}

impl LineAnchor {
    /// Formats the 2-character hash as a `String`.
    #[must_use]
    pub fn hash_str(self) -> String {
        crate::format_hash(self.hash)
    }
}

impl fmt::Display for LineAnchor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}#{}{}",
            self.line, self.hash[0] as char, self.hash[1] as char
        )
    }
}

/// A hash-anchored inclusive line range (`N#HH` or `N#HH.=M#KK`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnchorRange {
    /// Opening line anchor.
    pub start: LineAnchor,
    /// Closing line anchor (equal to `start` for a single-line target).
    pub end: LineAnchor,
}

impl AnchorRange {
    /// Constructs a single-line anchored range.
    #[must_use]
    pub const fn single(anchor: LineAnchor) -> Self {
        Self {
            start: anchor,
            end: anchor,
        }
    }

    /// Plain 1-indexed line span covered by this range.
    #[must_use]
    pub const fn lines(self) -> Range {
        Range {
            start: self.start.line,
            end: self.end.line,
        }
    }
}

/// Where a register paste lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasteTarget {
    /// Start of file (`<1`), allowed on empty or non-empty files without a hash.
    BeforeStart,
    /// The gap before an anchored line (`<N#HH`).
    Before(LineAnchor),
    /// The gap after an anchored line (`>N#HH`).
    After(LineAnchor),
    /// End of file (`>$`).
    Eof,
    /// The gap after the block beginning at an anchored line (`>N#HH*`).
    AfterBlock(LineAnchor),
    /// Over an anchored inclusive line range; requires a named register.
    Range(AnchorRange),
    /// Over the block beginning at an anchored line; requires a named register.
    Block(LineAnchor),
}

/// One operation inside a section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// `PUT N#HH:` / `PUT N#HH.=M#KK:` — replace the anchored range with `+` rows.
    Replace {
        /// Hash-anchored inclusive range.
        range: AnchorRange,
        /// Replacement lines.
        body: Vec<String>,
    },
    /// `PUT N#HH*:` — replace the block beginning on the anchored line.
    ReplaceBlock {
        /// Opening line anchor of the block.
        anchor: LineAnchor,
        /// Replacement lines.
        body: Vec<String>,
    },
    /// `PUT <1:` — insert at start of file without a hash (works on empty files).
    InsertBeforeStart {
        /// Lines to insert at file head.
        body: Vec<String>,
    },
    /// `PUT <N#HH:` — insert the body rows before the anchored line.
    InsertBefore {
        /// Target line anchor.
        anchor: LineAnchor,
        /// Lines to insert.
        body: Vec<String>,
    },
    /// `PUT >N#HH:` — insert the body rows after the anchored line.
    InsertAfter {
        /// Target line anchor.
        anchor: LineAnchor,
        /// Lines to insert.
        body: Vec<String>,
    },
    /// `PUT >$:` — append the body rows at end of file.
    InsertEof {
        /// Lines to append.
        body: Vec<String>,
    },
    /// `PUT >N#HH*:` — insert the body rows after the block at the anchored line.
    InsertAfterBlock {
        /// Opening line anchor of the block.
        anchor: LineAnchor,
        /// Lines to insert.
        body: Vec<String>,
    },
    /// `CUT N#HH [@reg]` / `CUT N#HH.=M#KK [@reg]` — delete lines and capture them.
    Cut {
        /// Hash-anchored inclusive range.
        range: AnchorRange,
        /// Optional named register.
        register: Option<String>,
    },
    /// `CUT N#HH* [@reg]` — delete the block at the anchored line and capture it.
    CutBlock {
        /// Opening line anchor of the block.
        anchor: LineAnchor,
        /// Optional named register.
        register: Option<String>,
    },
    /// `YANK R N#HH` / `YANK R N#HH.=M#KK` — copy lines into register `R` without deleting.
    Yank {
        /// Destination register name.
        register: String,
        /// Hash-anchored inclusive range.
        range: AnchorRange,
    },
    /// `YANK R N#HH*` — copy the block at the anchored line into register `R` without deleting.
    YankBlock {
        /// Destination register name.
        register: String,
        /// Opening line anchor of the block.
        anchor: LineAnchor,
    },
    /// `PASTE R …` or bodyless `PUT … [@reg]` — paste a register at a gap or over a span.
    Paste {
        /// Target gap or span.
        target: PasteTarget,
        /// Named register (`None` only for unlabeled `PUT` paste after unlabeled `CUT`).
        register: Option<String>,
    },
    /// `REM` — delete the file.
    Remove,
    /// `MV DEST` — write the result at DEST and remove the source.
    Move {
        /// Destination path relative to the working directory.
        dest: String,
    },
}

/// One `[path]` section: a file and the ops that address its lines.
#[derive(Debug, Clone)]
pub struct Section {
    /// The path as the header named it, relative to the working directory.
    pub path: String,
    /// The ops in patch order.
    pub ops: Vec<Op>,
}

/// The parsing state while a `PUT …:` header collects `+` body rows.
struct PendingBody {
    target: PutTarget,
    body: Vec<String>,
}

/// The target a `PUT` or `PASTE` addresses.
enum PutTarget {
    Range(AnchorRange),
    Block(LineAnchor),
    BeforeStart,
    Before(LineAnchor),
    After(LineAnchor),
    Eof,
    AfterBlock(LineAnchor),
}

impl PasteTarget {
    /// The paste form of a `PutTarget`, with the register rules applied.
    fn of(
        target: &PutTarget,
        register: Option<&str>,
        path: &str,
        line: usize,
    ) -> Result<Self, Error> {
        let named = register.is_some();
        Ok(match *target {
            PutTarget::BeforeStart => Self::BeforeStart,
            PutTarget::Before(anchor) => Self::Before(anchor),
            PutTarget::After(anchor) => Self::After(anchor),
            PutTarget::Eof => Self::Eof,
            PutTarget::AfterBlock(anchor) => Self::AfterBlock(anchor),
            PutTarget::Range(range) if named => Self::Range(range),
            PutTarget::Block(anchor) if named => Self::Block(anchor),
            PutTarget::Range(_) | PutTarget::Block(_) => {
                return Err(Error::Parse {
                    line,
                    detail: format!(
                        "{path}: missing trailing `:` on `PUT …:` (or for a span/block paste, use `PASTE R …` or `PUT … @R`)"
                    ),
                });
            }
        })
    }
}

/// Parses `patch` into its `[path]` sections.
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
                        detail: "a `+` body row needs a `PUT …:` header above it (use `PUT N#HH.=M#KK:` to replace lines; `CUT` deletes only)".to_string(),
                    });
                }
            }
            continue;
        }
        // Reject unified-diff `-old` rows explicitly with an actionable message.
        if raw.starts_with('-') {
            return Err(Error::Parse {
                line: line_no,
                detail: format!(
                    "unexpected `-` row `{raw}` — hashline patches have no `-old` diff lines; `PUT N#HH:` / `PUT N#HH.=M#KK:` replaces anchored lines with verbatim `+TEXT` rows (use `+-text` for a literal leading `-`)"
                ),
            });
        }
        if let Some(pending_body) = pending.take() {
            commit_pending(pending_body, sections.last_mut(), line_no)?;
        }
        if let Some(section) = parse_header(raw, line_no)? {
            sections.push(section);
            continue;
        }
        let Some(current) = sections.last_mut() else {
            return Err(Error::Parse {
                line: line_no,
                detail: "an op needs a `[path]` section header above it".to_string(),
            });
        };
        parse_op(raw, current, line_no, &mut pending)?;
    }
    if let Some(pending_body) = pending.take() {
        commit_pending(pending_body, sections.last_mut(), patch.lines().count())?;
    }
    if sections.is_empty() {
        return Err(Error::Parse {
            line: 0,
            detail: "the patch holds no `[path]` section".to_string(),
        });
    }
    for section in &sections {
        if section.ops.is_empty() {
            return Err(Error::Parse {
                line: 0,
                detail: format!(
                    "section `[{}]` has no ops — use `search sed -n '1,120p' {}` or `search rg` to read lines",
                    section.path, section.path
                ),
            });
        }
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
            detail: "a `PUT …:` header needs at least one `+` body row (`CUT` deletes, `PASTE` takes no body)".to_string(),
        });
    }
    let op = match pending.target {
        PutTarget::Range(range) => Op::Replace {
            range,
            body: pending.body,
        },
        PutTarget::Block(anchor) => Op::ReplaceBlock {
            anchor,
            body: pending.body,
        },
        PutTarget::BeforeStart => Op::InsertBeforeStart { body: pending.body },
        PutTarget::Before(anchor) => Op::InsertBefore {
            anchor,
            body: pending.body,
        },
        PutTarget::After(anchor) => Op::InsertAfter {
            anchor,
            body: pending.body,
        },
        PutTarget::Eof => Op::InsertEof { body: pending.body },
        PutTarget::AfterBlock(anchor) => Op::InsertAfterBlock {
            anchor,
            body: pending.body,
        },
    };
    section
        .expect("a pending PUT header only follows a section")
        .ops
        .push(op);
    Ok(())
}

/// Parses a `[path]` header line, returning `None` when not bracketed.
fn parse_header(raw: &str, line: usize) -> Result<Option<Section>, Error> {
    let trimmed = raw.trim();
    let Some(inner) = trimmed
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
    else {
        return Ok(None);
    };
    let mut path = inner;
    while let Some(rest) = path.strip_prefix("./") {
        path = rest;
    }
    if path.is_empty() || path.starts_with('/') || path.split('/').any(|segment| segment == "..") {
        return Err(Error::Parse {
            line,
            detail: "section paths are relative, without `..` segments".to_string(),
        });
    }
    Ok(Some(Section {
        path: path.to_string(),
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
            CutSpan::Block(anchor) => section.ops.push(Op::CutBlock { anchor, register }),
        }
        return Ok(());
    }
    if let Some(rest) = raw.strip_prefix("YANK ") {
        let Some((reg_raw, target)) = split_first_token(rest.trim()) else {
            return Err(fail(
                "expected `YANK R N#HH`, `YANK R N#HH.=M#KK`, or `YANK R N#HH*`".to_string(),
            ));
        };
        let register = parse_register_name(reg_raw, line, &path)?;
        match parse_span(target.trim(), line, &path)? {
            CutSpan::Range(range) => section.ops.push(Op::Yank { register, range }),
            CutSpan::Block(anchor) => section.ops.push(Op::YankBlock { register, anchor }),
        }
        return Ok(());
    }
    if let Some(rest) = raw.strip_prefix("PASTE ") {
        let Some((reg_raw, target)) = split_first_token(rest.trim()) else {
            return Err(fail(
                "expected `PASTE R <N#HH`, `PASTE R >N#HH`, `PASTE R <1`, `PASTE R >$`, `PASTE R N#HH.=M#KK`, or `PASTE R N#HH*`"
                    .to_string(),
            ));
        };
        let register = parse_register_name(reg_raw, line, &path)?;
        let target_str = target.trim();
        if target_str.ends_with(':') {
            return Err(fail(
                "`PASTE` takes no trailing `:` and no `+` body rows".to_string(),
            ));
        }
        let put_target = parse_put_target(target_str, line, &path)?;
        section.ops.push(Op::Paste {
            target: PasteTarget::of(&put_target, Some(&register), &path, line)?,
            register: Some(register),
        });
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
            target: PasteTarget::of(&put_target, register.as_deref(), &path, line)?,
            register,
        });
        return Ok(());
    }
    Err(fail(format!("unrecognized op `{raw}`")))
}

/// A `CUT` or `YANK` span: an anchored range or an anchored block.
enum CutSpan {
    Range(AnchorRange),
    Block(LineAnchor),
}

/// Parses `N#HH`, `N#HH.=M#KK`, or `N#HH*` for `CUT`/`YANK`.
fn parse_span(raw: &str, line: usize, path: &str) -> Result<CutSpan, Error> {
    if let Some(anchor) = raw.strip_suffix('*') {
        return Ok(CutSpan::Block(parse_anchor(anchor, line, path)?));
    }
    Ok(CutSpan::Range(parse_anchor_range(raw, line, path)?))
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

/// Splits the first whitespace-delimited token from the remainder.
fn split_first_token(raw: &str) -> Option<(&str, &str)> {
    let trimmed = raw.trim();
    let split = trimmed.find(char::is_whitespace)?;
    let (head, tail) = trimmed.split_at(split);
    let tail = tail.trim();
    if head.is_empty() || tail.is_empty() {
        return None;
    }
    Some((head, tail))
}

/// Validates a `YANK`/`PASTE` register name (accepting optional leading `@`).
fn parse_register_name(raw: &str, line: usize, path: &str) -> Result<String, Error> {
    let name = raw.strip_prefix('@').unwrap_or(raw);
    if name.is_empty()
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(Error::Parse {
            line,
            detail: format!(
                "{path}: invalid register name `{raw}` — expected an identifier like `r` (e.g. `YANK r 12#VK` / `PASTE r >20#PM`)"
            ),
        });
    }
    Ok(name.to_string())
}

/// Parses the target portion of a `PUT` or `PASTE` op.
fn parse_put_target(raw: &str, line: usize, path: &str) -> Result<PutTarget, Error> {
    let trimmed = raw.trim();
    if let Some(rest) = trimmed.strip_prefix('<') {
        let rest = rest.trim();
        if rest == "1" {
            return Ok(PutTarget::BeforeStart);
        }
        return parse_anchor(rest, line, path).map(PutTarget::Before);
    }
    if let Some(rest) = trimmed.strip_prefix('>') {
        let rest = rest.trim();
        if rest == "$" {
            return Ok(PutTarget::Eof);
        }
        if let Some(anchor) = rest.strip_suffix('*') {
            return parse_anchor(anchor, line, path).map(PutTarget::AfterBlock);
        }
        return parse_anchor(rest, line, path).map(PutTarget::After);
    }
    if let Some(anchor) = trimmed.strip_suffix('*') {
        return parse_anchor(anchor, line, path).map(PutTarget::Block);
    }
    parse_anchor_range(trimmed, line, path).map(PutTarget::Range)
}

/// Parses `N#HH` or `N#HH.=M#KK` as a hash-anchored inclusive range.
fn parse_anchor_range(raw: &str, line: usize, path: &str) -> Result<AnchorRange, Error> {
    if let Some((a, b)) = raw.split_once(".=") {
        let start = parse_anchor(a, line, path)?;
        let end = parse_anchor(b, line, path)?;
        if start.line > end.line {
            return Err(Error::Parse {
                line,
                detail: format!(
                    "{path}: invalid range `{raw}` — start line {} exceeds end line {}",
                    start.line, end.line
                ),
            });
        }
        Ok(AnchorRange { start, end })
    } else {
        Ok(AnchorRange::single(parse_anchor(raw, line, path)?))
    }
}

/// Parses `N#HH` where `N >= 1` and `HH` is two characters from [`LINE_HASH_ALPHABET`]
/// (lowercase accepted and normalized to uppercase).
fn parse_anchor(raw: &str, line: usize, path: &str) -> Result<LineAnchor, Error> {
    let trimmed = raw.trim();
    let Some((num_raw, hash_raw)) = trimmed.split_once('#') else {
        return Err(Error::Parse {
            line,
            detail: format!(
                "{path}: line anchor `{trimmed}` is missing its 2-char `#HH` hash — copy `N#HH` from `search` output (e.g. `PUT 12#VK:`)"
            ),
        });
    };
    let line_num = num_raw
        .parse::<usize>()
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(|| Error::Parse {
            line,
            detail: format!(
                "{path}: `{num_raw}` in `{trimmed}` is not a 1-indexed line number (e.g. `PUT 12#VK:`)"
            ),
        })?;
    let bytes = hash_raw.as_bytes();
    if bytes.len() != 2 {
        return Err(Error::Parse {
            line,
            detail: format!(
                "{path}: invalid line hash `#{hash_raw}` in `{trimmed}` — expected 2 chars from `ZPMQVRWSNKTXJBYH` (e.g. `PUT 12#VK:`)"
            ),
        });
    }
    let h0 = bytes[0].to_ascii_uppercase();
    let h1 = bytes[1].to_ascii_uppercase();
    if !LINE_HASH_ALPHABET.contains(&h0) || !LINE_HASH_ALPHABET.contains(&h1) {
        return Err(Error::Parse {
            line,
            detail: format!(
                "{path}: invalid line hash `#{hash_raw}` in `{trimmed}` — expected 2 chars from `ZPMQVRWSNKTXJBYH` (e.g. `PUT 12#VK:`)"
            ),
        });
    }
    Ok(LineAnchor {
        line: line_num,
        hash: [h0, h1],
    })
}

/// Removes one level of `"…"` quoting from a path argument.
fn unquote(raw: &str) -> String {
    match raw.strip_prefix('"').and_then(|r| r.strip_suffix('"')) {
        Some(inner) => inner.to_string(),
        None => raw.to_string(),
    }
}
