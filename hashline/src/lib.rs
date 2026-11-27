#![allow(clippy::disallowed_methods)]
//! Engine behind the `search` and `edit` stateless per-line hashline executables.
//!
//! File text served by `search` is wrapped in `[path]` sections of
//! `N#HH|content` lines, where `HH` is a 2-character content-based hash over
//! the unambiguous alphabet `ZPMQVRWSNKTXJBYH`. Edits reference those exact
//! `N#HH` anchors and are verified atomically before any file write lands.
//!
//! All work happens inside the calling process, so the binaries inherit
//! whatever filesystem sandbox the launching shell enforces; there is no IPC,
//! no session state file, and no privileged helper.

mod apply;
mod block;
mod error;
mod parse;
mod read;
mod rg;
mod sed;
mod sg;

use std::path::Path;

pub use apply::{
    ApplyReport, FileOutcome, SectionReport, apply_patch, format_hunk_echo, report_header,
};
pub use block::{AstResolver, BlockResolver};
pub use error::Error;
pub use parse::{AnchorRange, LineAnchor, Op, PasteTarget, Range, Section, parse};
pub use read::{HeadMode, ReadMode, ReadOptions, TailMode, run_read};
pub use rg::{
    CaseMode, RgOptions, RgOutputMode, RgRunResult, SortOrder, run_rg, run_rg_many, type_list,
};
pub use sed::{SedOptions, run_sed};
pub use sg::{SgOptions, run_sg};

/// Unambiguous 16-character nibble alphabet for per-line 2-character hashes.
pub const LINE_HASH_ALPHABET: &[u8; 16] = b"ZPMQVRWSNKTXJBYH";

/// Computes the 2-character content-based hash of a single line.
///
/// Trailing `\r` is stripped so CRLF and LF lines hash identically, while
/// leading and inner whitespace are preserved verbatim so indentation changes
/// invalidate the hash. The hash depends solely on line content and never on
/// line number, so shifting lines elsewhere never changes an untouched line's
/// hash.
#[must_use]
pub fn line_hash(line: &str) -> [u8; 2] {
    let clean = line.trim_end_matches('\r');
    let h = xxhash_rust::xxh32::xxh32(clean.as_bytes(), 0);
    let b = (h & 0xFF) as usize;
    [
        LINE_HASH_ALPHABET[(b >> 4) & 0x0F],
        LINE_HASH_ALPHABET[b & 0x0F],
    ]
}

/// Formats a 2-character nibble hash array as a `String`.
#[must_use]
pub fn format_hash(hash: [u8; 2]) -> String {
    let mut s = String::with_capacity(2);
    s.push(hash[0] as char);
    s.push(hash[1] as char);
    s
}

/// Computes the 2-character content-based hash of `line` as a `String`.
#[must_use]
pub fn line_hash_str(line: &str) -> String {
    format_hash(line_hash(line))
}

/// Strips any redundant leading `./` so section headers stay canonical (`src/a.rs`).
#[must_use]
pub fn display_path(path: &Path) -> String {
    let raw = path.display().to_string();
    let mut trimmed = raw.as_str();
    while let Some(rest) = trimmed.strip_prefix("./") {
        trimmed = rest;
    }
    trimmed.to_string()
}

/// Formats a section header as `[path]`.
#[must_use]
pub fn section_header(path: &str) -> String {
    format!("[{path}]")
}

/// Formats a single anchored line as `N#HH|content`.
#[must_use]
pub fn hashed_line(line_no: usize, line: &str) -> String {
    let clean = line.trim_end_matches('\r');
    format!("{line_no}#{}|{clean}", line_hash_str(clean))
}

/// Splits file text into 1-indexed display lines (trailing `\n` omitted,
/// per-line trailing `\r` stripped; an empty file yields an empty `Vec`).
#[must_use]
pub fn split_display_lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&str> = text.split('\n').collect();
    if text.ends_with('\n') {
        lines.pop();
    }
    for line in &mut lines {
        *line = line.strip_suffix('\r').unwrap_or(line);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &tempfile::TempDir, name: &str, text: &str) {
        let path = dir.path().join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, text).unwrap();
    }

    fn apply(dir: &tempfile::TempDir, patch: &str) -> Result<ApplyReport, Error> {
        apply_patch(dir.path(), patch, &AstResolver)
    }

    fn read(dir: &tempfile::TempDir, name: &str) -> String {
        std::fs::read_to_string(dir.path().join(name)).unwrap()
    }

    fn anchor(line_no: usize, line: &str) -> String {
        format!("{line_no}#{}", line_hash_str(line))
    }

    fn wrong_hash(line: &str) -> String {
        let actual = line_hash(line);
        let alt0 = LINE_HASH_ALPHABET
            .iter()
            .copied()
            .find(|&b| b != actual[0])
            .unwrap();
        format_hash([alt0, actual[1]])
    }

    #[test]
    fn line_hash_uses_alphabet_is_shift_invariant_and_indentation_sensitive() {
        for sample in [
            "",
            "    let x = 1;",
            "  let x = 1;",
            "let x = 1;",
            "中文 / 日本語 / 한국어",
            "trailing cr\r",
        ] {
            let h = line_hash(sample);
            assert!(
                LINE_HASH_ALPHABET.contains(&h[0]) && LINE_HASH_ALPHABET.contains(&h[1]),
                "hash {:?} not in alphabet for `{sample}`",
                format_hash(h)
            );
        }
        // CRLF invariance: trailing \r does not change hash.
        assert_eq!(line_hash("fn main() {}"), line_hash("fn main() {}\r"));
        // Purely content-based: identical content hashes identically regardless of line number.
        let before = "alpha\nbeta\ntarget_line();\ngamma\n";
        let after = "added1\nadded2\nalpha\nbeta\ntarget_line();\ngamma\n";
        let before_lines = split_display_lines(before);
        let after_lines = split_display_lines(after);
        assert_eq!(line_hash(before_lines[2]), line_hash(after_lines[4]));
        assert_eq!(
            hashed_line(3, "fn main() {}\r"),
            format!("3#{}|fn main() {{}}", line_hash_str("fn main() {}"))
        );
        // Indentation sensitivity: different leading whitespace produces different xxh32 input.
        let mut saw_indent_diff = false;
        for i in 0..20 {
            let a = format!("    let item_{i} = {i};");
            let b = format!("  let item_{i} = {i};");
            if line_hash(&a) != line_hash(&b) {
                saw_indent_diff = true;
                break;
            }
        }
        assert!(
            saw_indent_diff,
            "indentation differences must affect line_hash"
        );
    }

    #[test]
    fn put_single_range_insert_before_after_bof_and_eof() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir, "a.txt", "one\ntwo\nthree\nfour\n");
        let patch = format!(
            "[a.txt]\n\
             PUT {}:\n+TWO\n\
             PUT <1:\n+zero\n\
             PUT >$:\n+five\n",
            anchor(2, "two")
        );
        let report = apply(&dir, &patch).unwrap();
        assert_eq!(read(&dir, "a.txt"), "zero\none\nTWO\nthree\nfour\nfive\n");
        assert!(matches!(report.sections[0].outcome, FileOutcome::Modified));
        assert_eq!(report.sections[0].line_delta, 2);

        // Also test range replace, insert before/after anchored lines, and `<1#HH:`.
        let patch2 = format!(
            "[a.txt]\n\
             PUT <{}:\n+start\n\
             PUT {}.={}:\n+MID\n\
             PUT >{}:\n+after_four\n",
            anchor(1, "zero").to_ascii_lowercase(), // lowercase hashes accepted
            anchor(3, "TWO"),
            anchor(4, "three"),
            anchor(5, "four"),
        );
        apply(&dir, &patch2).unwrap();
        assert_eq!(
            read(&dir, "a.txt"),
            "start\nzero\none\nMID\nfour\nafter_four\nfive\n"
        );
    }

    #[test]
    fn put_bof_and_eof_work_on_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir, "empty.txt", "");
        apply(&dir, "[empty.txt]\nPUT <1:\n+first\nPUT >$:\n+second\n").unwrap();
        assert_eq!(read(&dir, "empty.txt"), "first\nsecond\n");

        // `<1#HH:` on an empty file must fail with out-of-bounds mismatch.
        write(&dir, "empty2.txt", "");
        let err = apply(&dir, "[empty2.txt]\nPUT <1#PM:\n+first\n").unwrap_err();
        let msg = err.to_string();
        assert!(matches!(err, Error::HashMismatch { .. }));
        assert!(msg.contains("file only has 0 lines"), "msg: {msg}");
        assert!(msg.contains("(empty file)"), "msg: {msg}");
    }

    #[test]
    fn cut_yank_and_paste_registers_including_cross_file() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir, "a.txt", "one\ntwo\nthree\nfour\n");
        write(&dir, "b.txt", "alpha\nbeta\n");

        // YANK keeps source intact; PASTE copies into b.txt and replaces range in a.txt.
        let patch = format!(
            "[a.txt]\n\
             YANK keep {}.={}\n\
             CUT {} @m\n\
             PASTE m >{}\n\
             [b.txt]\n\
             PASTE keep <1\n\
             PASTE keep >$\n",
            anchor(2, "two"),
            anchor(3, "three"),
            anchor(1, "one"),
            anchor(4, "four"),
        );
        apply(&dir, &patch).unwrap();
        assert_eq!(read(&dir, "a.txt"), "two\nthree\nfour\none\n");
        assert_eq!(read(&dir, "b.txt"), "two\nthree\nalpha\nbeta\ntwo\nthree\n");

        // PASTE over a range replaces it.
        let patch2 = format!(
            "[b.txt]\n\
             YANK head {}\n\
             PASTE head {}.={}\n",
            anchor(3, "alpha"),
            anchor(5, "two"),
            anchor(6, "three"),
        );
        apply(&dir, &patch2).unwrap();
        assert_eq!(read(&dir, "b.txt"), "two\nthree\nalpha\nbeta\nalpha\n");
    }

    #[test]
    fn rem_and_mv_operate_on_files() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir, "a.txt", "one\ntwo\n");
        let report = apply(
            &dir,
            &format!("[a.txt]\nMV sub/b.txt\nPUT {}:\n+TWO\n", anchor(2, "two")),
        )
        .unwrap();
        assert_eq!(read(&dir, "sub/b.txt"), "one\nTWO\n");
        assert!(!dir.path().join("a.txt").exists());
        assert_eq!(report_header(&report.sections[0]), "[sub/b.txt]");

        write(&dir, "c.txt", "gone\n");
        let rem_report = apply(&dir, "[c.txt]\nREM\n").unwrap();
        assert!(!dir.path().join("c.txt").exists());
        assert_eq!(report_header(&rem_report.sections[0]), "[c.txt] (removed)");
    }

    #[test]
    fn block_put_cut_yank_and_paste_operate_on_ast_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let src = "fn a() {\n    one();\n    two();\n}\n\nfn b() {\n    three();\n}\n";
        write(&dir, "f.rs", src);
        apply(
            &dir,
            &format!(
                "[f.rs]\n\
                 YANK fn_b {}*\n\
                 PUT {}*:\n\
                 +fn a() {{\n\
                 +    redone();\n\
                 +}}\n\
                 CUT {}*\n\
                 PASTE fn_b >$\n",
                anchor(6, "fn b() {"),
                anchor(1, "fn a() {"),
                anchor(6, "fn b() {"),
            ),
        )
        .unwrap();
        assert_eq!(
            read(&dir, "f.rs"),
            "fn a() {\n    redone();\n}\n\nfn b() {\n    three();\n}\n"
        );
    }

    #[test]
    fn mismatch_refuses_atomically_collects_all_mismatches_and_gives_moved_line_hints() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir, "a.txt", "ok_a\n");
        let mut lines = Vec::new();
        for i in 1..=30 {
            lines.push(format!("unique_line_{i}"));
        }
        // Duplicate a distinct line twice (at 20 and 28) and four blank lines (22..=25).
        lines[19] = "dup_two_times".to_string();
        lines[27] = "dup_two_times".to_string();
        lines[21] = String::new();
        lines[22] = String::new();
        lines[23] = String::new();
        lines[24] = String::new();
        let b_text = format!("{}\n", lines.join("\n"));
        write(&dir, "b.txt", &b_text);

        // 1. Unique move hint: `unique_line_15` is at line 15, addressed wrongly at line 5.
        // 2. Multi-match hint (2 matches at 20 and 28): addressed wrongly at line 26 -> closest is 28, then 20.
        // 3. >3 matches (blank lines at 22..=25): addressed wrongly at line 1 -> NO hint!
        // 4. Out-of-bounds line 99 -> reports total lines.
        let h_moved = line_hash_str("unique_line_15");
        let h_dup = line_hash_str("dup_two_times");
        let h_blank = line_hash_str("");
        let h_oob = wrong_hash("anything");

        let patch = format!(
            "[a.txt]\n\
             PUT {}:\n+CHANGED_A\n\
             [b.txt]\n\
             PUT 5#{h_moved}.=26#{h_dup}:\n+x\n\
             PUT 1#{h_blank}:\n+y\n\
             PUT 99#{h_oob}:\n+z\n",
            anchor(1, "ok_a"),
        );
        let err = apply(&dir, &patch).unwrap_err();
        assert!(matches!(err, Error::HashMismatch { .. }));
        let msg = err.to_string();

        // Atomic rollback: a.txt MUST NOT be modified!
        assert_eq!(read(&dir, "a.txt"), "ok_a\n");
        assert_eq!(read(&dir, "b.txt"), b_text);

        // Check all 4 mismatches reported together.
        assert!(
            msg.contains(&format!(
                "expected 5#{h_moved}, found 5#{} (hint: #{h_moved} is at line 15)",
                line_hash_str("unique_line_5")
            )),
            "msg:\n{msg}"
        );
        assert!(
            msg.contains(&format!(
                "expected 26#{h_dup}, found 26#{} (hint: #{h_dup} matches lines 28, 20)",
                line_hash_str("unique_line_26")
            )),
            "msg:\n{msg}"
        );
        assert!(
            msg.contains(&format!(
                "expected 1#{h_blank}, found 1#{}",
                line_hash_str("unique_line_1")
            )),
            "msg:\n{msg}"
        );
        assert!(
            !msg.contains(&format!("hint: #{h_blank}")),
            ">3 matches must omit hint:\n{msg}"
        );
        assert!(
            msg.contains(&format!("expected 99#{h_oob}, but file only has 30 lines")),
            "msg:\n{msg}"
        );

        // Context displays [b.txt] + live ±3 around mismatched lines and ±2 around hinted moved lines.
        assert!(msg.contains("[b.txt]\n"), "msg:\n{msg}");
        assert!(
            msg.contains(&hashed_line(15, "unique_line_15")),
            "hinted line 15 must appear in context:\n{msg}"
        );
        assert!(
            msg.contains(&hashed_line(13, "unique_line_13")),
            "±2 around hinted line 15 must appear:\n{msg}"
        );
        assert!(
            msg.contains(&hashed_line(28, "dup_two_times")),
            "closest multi-match line 28 must appear:\n{msg}"
        );
    }

    #[test]
    fn preserves_crlf_and_missing_trailing_newline() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir, "win.txt", "alpha\r\nbeta\r\ngamma\r\n");
        apply(
            &dir,
            &format!("[win.txt]\nPUT {}:\n+BETA\n+BETA2\n", anchor(2, "beta")),
        )
        .unwrap();
        assert_eq!(read(&dir, "win.txt"), "alpha\r\nBETA\r\nBETA2\r\ngamma\r\n");

        write(&dir, "notrail.txt", "one\ntwo");
        apply(
            &dir,
            &format!("[notrail.txt]\nPUT {}:\n+TWO\n", anchor(2, "two")),
        )
        .unwrap();
        assert_eq!(read(&dir, "notrail.txt"), "one\nTWO");
    }

    #[test]
    fn rejects_invalid_anchors_minus_rows_and_legacy_section_tag() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir, "a.txt", "one\ntwo\n");

        // Missing `#HH`
        let err = parse("[a.txt]\nPUT 1:\n+x\n").unwrap_err();
        assert!(err.to_string().contains("PUT 12#VK:"), "err: {err}");

        // Invalid char (`A` not in ZPMQVRWSNKTXJBYH)
        let err = parse("[a.txt]\nPUT 1#AA:\n+x\n").unwrap_err();
        assert!(err.to_string().contains("ZPMQVRWSNKTXJBYH"), "err: {err}");

        // `-old` row rejected explicitly
        let err = parse(&format!("[a.txt]\nPUT {}:\n-one\n+ONE\n", anchor(1, "one"))).unwrap_err();
        assert!(err.to_string().contains("-old"), "err: {err}");

        // Legacy `[a.txt#1A2B]` section header gives actionable error when `a.txt` exists
        let err = apply(
            &dir,
            &format!("[a.txt#1A2B]\nPUT {}:\n+ONE\n", anchor(1, "one")),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("[a.txt]") && err.to_string().contains("N#HH"),
            "err: {err}"
        );
    }

    #[test]
    fn body_rows_keep_verbatim_leading_signs_and_overlapping_ops_fail() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir, "a.txt", "x\ny\n");
        apply(
            &dir,
            &format!(
                "[a.txt]\nPUT {}:\n++plus\n+-minus\n+  indent\n+\n",
                anchor(1, "x")
            ),
        )
        .unwrap();
        assert_eq!(read(&dir, "a.txt"), "+plus\n-minus\n  indent\n\ny\n");

        // Overlapping ops must fail without writing.
        let before = read(&dir, "a.txt");
        let err = apply(
            &dir,
            &format!(
                "[a.txt]\nPUT {}.={}:\n+A\nCUT {}\n",
                anchor(1, "+plus"),
                anchor(2, "-minus"),
                anchor(2, "-minus"),
            ),
        )
        .unwrap_err();
        assert!(matches!(err, Error::Anchoring { .. }));
        assert_eq!(read(&dir, "a.txt"), before);
    }

    #[test]
    fn paste_needs_its_register_filled() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir, "a.txt", "one\n");
        let err = apply(
            &dir,
            &format!("[a.txt]\nPASTE missing >{}\n", anchor(1, "one")),
        )
        .unwrap_err();
        assert!(matches!(err, Error::EmptyRegister { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn editing_workspace_root_succeeds_under_readonly_parent_dir() {
        use std::os::unix::fs::PermissionsExt as _;

        let outer = tempfile::tempdir().unwrap();
        let workspace = outer.path().join("repo");
        std::fs::create_dir(&workspace).unwrap();
        let file = workspace.join("sample.py");
        std::fs::write(&file, "alpha = 1\nbeta = 2\n").unwrap();

        std::fs::set_permissions(outer.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
        let res = apply_patch(
            &workspace,
            &format!("[sample.py]\nPUT {}:\n+beta = 42\n", anchor(2, "beta = 2")),
            &AstResolver,
        );
        std::fs::set_permissions(outer.path(), std::fs::Permissions::from_mode(0o755)).unwrap();

        res.unwrap();
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "alpha = 1\nbeta = 42\n"
        );
    }
}
