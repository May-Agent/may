//! Engine behind the `search` and `edit` hashline executables.
//!
//! Hashline is a hash-anchored line protocol: file text the tools serve is
//! wrapped in `[path#TAG]` sections of `N:content` lines, where the tag is a
//! content hash of the whole file, and edits address the original numbered
//! lines the model was shown. A tag that no longer matches the live file
//! refuses the edit instead of landing it on shifted content.
//!
//! All work happens inside the calling process, so the binaries inherit
//! whatever filesystem sandbox the launching shell enforces; there is no IPC
//! and no privileged helper.

mod apply;
mod block;
mod error;
mod parse;
mod rg;
mod sg;

use std::fmt::Write as _;

pub use apply::{ApplyReport, FileOutcome, SectionReport, apply_patch, report_header};
pub use block::{AstResolver, BlockResolver};
pub use error::Error;
pub use parse::{Op, PasteTarget, Range, Section, parse};
pub use rg::{RgOptions, run_rg};
pub use sg::{SgOptions, run_sg};

/// Number of hex characters in a content-derived file tag.
pub const FILE_TAG_LEN: usize = 4;

/// Trailing whitespace never invalidates a tag: it is stripped before hashing
/// so CRLF endings and display-trimmed lines still match.
fn normalize_hash_text(text: &str) -> String {
    text.split('\n')
        .map(|line| line.trim_end_matches([' ', '\t', '\r']))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The 4-hex content fingerprint a section header carries. A tag minted on a
/// `search`/`read` row validates a follow-up `edit` while the live file still
/// hashes to it.
#[must_use]
pub fn file_tag(text: &str) -> String {
    let hash = xxhash_rust::xxh32::xxh32(normalize_hash_text(text).as_bytes(), 0) & 0xFFFF;
    format!("{:04X}", hash)
}

/// Formats a hashline section header for a file path and tag.
#[must_use]
pub fn section_header(path: &str, tag: &str) -> String {
    format!("[{path}#{tag}]")
}

/// Formats a single numbered line as `LINE:TEXT`.
#[must_use]
pub fn numbered_line(number: usize, line: &str) -> String {
    format!("{number}:{line}")
}

/// Appends `LINE:TEXT` rows for lines `start..=end` (1-indexed) to `out`.
pub fn append_numbered_lines(out: &mut String, lines: &[String], start: usize, end: usize) {
    for (offset, line) in lines[start.saturating_sub(1)..end].iter().enumerate() {
        let _ = writeln!(out, "{}", numbered_line(start + offset, line));
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    fn write(dir: &tempfile::TempDir, name: &str, text: &str) -> String {
        let path = dir.path().join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, text).unwrap();
        file_tag(text)
    }

    fn apply(dir: &tempfile::TempDir, patch: &str) -> Result<ApplyReport, Error> {
        apply_patch(dir.path(), patch, &AstResolver)
    }

    fn read(dir: &tempfile::TempDir, name: &str) -> String {
        std::fs::read_to_string(dir.path().join(name)).unwrap()
    }

    #[test]
    fn the_tag_is_four_hex_chars_of_normalized_content() {
        let tag = file_tag("a\nb\n");
        assert_eq!(tag.len(), 4);
        assert!(tag.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(tag, file_tag("a\r\nb\r\n"));
        assert_ne!(tag, file_tag("a\nb\nx"));
    }

    #[test]
    fn put_replaces_lines_and_inserts_at_gaps() {
        let dir = tempfile::tempdir().unwrap();
        let tag = write(&dir, "a.txt", "one\ntwo\nthree\nfour\n");
        let report = apply(
            &dir,
            &format!("[a.txt#{tag}]\nPUT 2:\n+TWO\nPUT <1:\n+zero\nPUT >$:\n+five\n"),
        )
        .unwrap();
        assert_eq!(read(&dir, "a.txt"), "zero\none\nTWO\nthree\nfour\nfive\n");
        assert!(matches!(report.sections[0].outcome, FileOutcome::Modified));
        assert_eq!(
            report.sections[0].new_tag.as_deref(),
            Some(file_tag(&read(&dir, "a.txt")).as_str())
        );
    }

    #[test]
    fn a_stale_tag_refuses_the_whole_patch() {
        let dir = tempfile::tempdir().unwrap();
        let tag = write(&dir, "a.txt", "one\n");
        write(&dir, "b.txt", "x\n");
        let error = apply(&dir, "[b.txt#DEAD]\nPUT 1:\n+y\n").unwrap_err();
        assert!(matches!(error, Error::StaleTag { .. }));
        assert_eq!(read(&dir, "b.txt"), "x\n");
        let error = apply(
            &dir,
            &format!("[a.txt#{tag}]\nPUT 1:\n+A\n[b.txt#DEAD]\nPUT 1:\n+y\n"),
        )
        .unwrap_err();
        assert!(matches!(error, Error::StaleTag { .. }));
        assert_eq!(read(&dir, "a.txt"), "one\n");
    }

    #[test]
    fn cut_paste_moves_lines_through_a_register() {
        let dir = tempfile::tempdir().unwrap();
        let tag = write(&dir, "a.txt", "one\ntwo\nthree\nfour\n");
        apply(&dir, &format!("[a.txt#{tag}]\nCUT 2.=3 @m\nPUT >4 @m\n")).unwrap();
        assert_eq!(read(&dir, "a.txt"), "one\nfour\ntwo\nthree\n");
    }

    #[test]
    fn rem_and_mv_operate_on_files() {
        let dir = tempfile::tempdir().unwrap();
        let tag = write(&dir, "a.txt", "one\ntwo\n");
        apply(
            &dir,
            &format!("[a.txt#{tag}]\nMV sub/b.txt\nPUT 2:\n+TWO\n"),
        )
        .unwrap();
        assert_eq!(read(&dir, "sub/b.txt"), "one\nTWO\n");
        assert!(!dir.path().join("a.txt").exists());
        let tag = write(&dir, "c.txt", "gone\n");
        apply(&dir, &format!("[c.txt#{tag}]\nREM\n")).unwrap();
        assert!(!dir.path().join("c.txt").exists());
    }

    #[test]
    fn a_block_put_replaces_the_whole_function() {
        let dir = tempfile::tempdir().unwrap();
        let src = "fn a() {\n    one();\n    two();\n}\n\nfn b() {\n    three();\n}\n";
        let tag = write(&dir, "f.rs", src);
        apply(
            &dir,
            &format!("[f.rs#{tag}]\nPUT 1*:\n+fn a() {{\n+    redone();\n+}}\n"),
        )
        .unwrap();
        assert_eq!(
            read(&dir, "f.rs"),
            "fn a() {\n    redone();\n}\n\nfn b() {\n    three();\n}\n"
        );
    }

    #[test]
    fn a_paste_needs_its_register_filled() {
        let dir = tempfile::tempdir().unwrap();
        let tag = write(&dir, "a.txt", "one\n");
        let error = apply(&dir, &format!("[a.txt#{tag}]\nPUT >1 @missing\n")).unwrap_err();
        assert!(matches!(error, Error::EmptyRegister { .. }));
    }

    #[test]
    fn body_rows_keep_verbatim_leading_signs() {
        let dir = tempfile::tempdir().unwrap();
        let tag = write(&dir, "a.txt", "x\n");
        apply(
            &dir,
            &format!("[a.txt#{tag}]\nPUT 1:\n++plus\n+-minus\n+  indent\n+\n"),
        )
        .unwrap();
        assert_eq!(read(&dir, "a.txt"), "+plus\n-minus\n  indent\n\n");
    }
}
