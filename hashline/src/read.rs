//! `search cat`, `search head`, `search tail`, and `search nl`: read-only file
//! readers whose output carries per-line hashline anchors (`[path]` + `N#HH|content`).

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::error::Error;
use crate::{display_path, hashed_line, section_header, split_display_lines};

/// Line selection mode for `search head`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadMode {
    /// Emit lines `1..=n`.
    First(usize),
    /// Emit all lines except the last `n` (`head -n -N`).
    AllButLast(usize),
}

/// Line selection mode for `search tail`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TailMode {
    /// Emit the last `n` lines of the file.
    Last(usize),
    /// Emit lines starting at 1-indexed line `n` through EOF (`tail -n +N`).
    FromLine(usize),
}

/// Which read subcommand is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadMode {
    /// Whole file (`search cat`).
    Cat,
    /// Leading lines (`search head`).
    Head(HeadMode),
    /// Trailing lines or tail-from-line (`search tail`).
    Tail(TailMode),
    /// Whole file with line numbers (`search nl`).
    Nl,
}

impl ReadMode {
    /// Subcommand name for error messages.
    const fn label(self) -> &'static str {
        match self {
            Self::Cat => "search cat",
            Self::Head(_) => "search head",
            Self::Tail(_) => "search tail",
            Self::Nl => "search nl",
        }
    }
}

/// Options shared by `search cat`, `search head`, `search tail`, and `search nl`.
#[derive(Debug, Clone, Default)]
pub struct ReadOptions {
    /// Maximum total output lines across all files (`None` = unlimited).
    pub limit: Option<usize>,
}

/// Runs a read subcommand over `paths` and returns hashline-formatted output.
///
/// # Errors
/// Returns [`Error::Usage`] when no files are provided or stdin (`-`) is named,
/// and [`Error::Io`] when a file cannot be read.
pub fn run_read(mode: ReadMode, paths: &[PathBuf], opts: &ReadOptions) -> Result<String, Error> {
    if paths.is_empty() {
        return Err(Error::Usage(format!(
            "{}: at least one file path is required (stdin pipelines are not supported)",
            mode.label()
        )));
    }
    for path in paths {
        if path.as_os_str() == "-" {
            return Err(Error::Usage(format!(
                "{}: stdin (`-`) is not supported — pass file paths to get hashline anchors",
                mode.label()
            )));
        }
    }

    let mut out = String::new();
    let mut remaining = opts.limit.unwrap_or(usize::MAX);
    for path in paths {
        if remaining == 0 {
            break;
        }
        emit_file(mode, &mut out, path, &mut remaining)?;
    }
    Ok(out)
}

fn emit_file(
    mode: ReadMode,
    out: &mut String,
    path: &Path,
    remaining: &mut usize,
) -> Result<(), Error> {
    let bytes = std::fs::read(path).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let text = String::from_utf8_lossy(&bytes);
    let display = display_path(path);
    let lines = split_display_lines(&text);
    let total = lines.len();

    if total == 0 {
        let should_show_empty = match mode {
            ReadMode::Cat | ReadMode::Nl => true,
            ReadMode::Head(HeadMode::First(n)) => n > 0,
            ReadMode::Head(HeadMode::AllButLast(_)) => true,
            ReadMode::Tail(TailMode::Last(n)) => n > 0,
            ReadMode::Tail(TailMode::FromLine(n)) => n <= 1,
        };
        if should_show_empty {
            let _ = writeln!(out, "{}", section_header(&display));
            let _ = writeln!(out, "(empty file)");
        }
        return Ok(());
    }

    let Some((start, end)) = selected_bounds(mode, total) else {
        return Ok(());
    };

    let _ = writeln!(out, "{}", section_header(&display));
    for line_no in start..=end {
        if *remaining == 0 {
            break;
        }
        let _ = writeln!(out, "{}", hashed_line(line_no, lines[line_no - 1]));
        *remaining -= 1;
    }
    Ok(())
}

/// Computes the 1-indexed inclusive `(start, end)` range to print, or `None`
/// when the selection produces zero lines.
fn selected_bounds(mode: ReadMode, total: usize) -> Option<(usize, usize)> {
    if total == 0 {
        return None;
    }
    match mode {
        ReadMode::Cat | ReadMode::Nl => Some((1, total)),
        ReadMode::Head(HeadMode::First(0)) | ReadMode::Tail(TailMode::Last(0)) => None,
        ReadMode::Head(HeadMode::First(n)) => Some((1, n.min(total))),
        ReadMode::Head(HeadMode::AllButLast(n)) => {
            let end = total.saturating_sub(n);
            (end > 0).then_some((1, end))
        }
        ReadMode::Tail(TailMode::Last(n)) => {
            let start = total.saturating_sub(n) + 1;
            Some((start, total))
        }
        ReadMode::Tail(TailMode::FromLine(n)) => {
            let start = n.max(1);
            (start <= total).then_some((start, total))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_file(dir: &tempfile::TempDir, name: &str, text: &str) -> PathBuf {
        let path = dir.path().join(name);
        std::fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn cat_head_tail_and_nl_emit_real_line_numbers_and_hashes() {
        let dir = tempfile::tempdir().unwrap();
        let text = "l1\nl2\nl3\nl4\nl5\nl6\n";
        let path = write_file(&dir, "demo.txt", text);

        let out = run_read(
            ReadMode::Tail(TailMode::Last(2)),
            std::slice::from_ref(&path),
            &ReadOptions::default(),
        )
        .unwrap();
        assert_eq!(
            out,
            format!(
                "[{}]\n{}\n{}\n",
                path.display(),
                hashed_line(5, "l5"),
                hashed_line(6, "l6")
            )
        );

        let out = run_read(
            ReadMode::Tail(TailMode::FromLine(4)),
            std::slice::from_ref(&path),
            &ReadOptions::default(),
        )
        .unwrap();
        assert_eq!(
            out,
            format!(
                "[{}]\n{}\n{}\n{}\n",
                path.display(),
                hashed_line(4, "l4"),
                hashed_line(5, "l5"),
                hashed_line(6, "l6")
            )
        );

        let out = run_read(
            ReadMode::Head(HeadMode::First(2)),
            std::slice::from_ref(&path),
            &ReadOptions::default(),
        )
        .unwrap();
        assert_eq!(
            out,
            format!(
                "[{}]\n{}\n{}\n",
                path.display(),
                hashed_line(1, "l1"),
                hashed_line(2, "l2")
            )
        );

        let out = run_read(
            ReadMode::Cat,
            std::slice::from_ref(&path),
            &ReadOptions { limit: Some(3) },
        )
        .unwrap();
        assert_eq!(
            out,
            format!(
                "[{}]\n{}\n{}\n{}\n",
                path.display(),
                hashed_line(1, "l1"),
                hashed_line(2, "l2"),
                hashed_line(3, "l3")
            )
        );

        // Empty file emits `[path]\n(empty file)\n`.
        let empty = write_file(&dir, "empty.txt", "");
        let out = run_read(
            ReadMode::Cat,
            std::slice::from_ref(&empty),
            &ReadOptions::default(),
        )
        .unwrap();
        assert_eq!(out, format!("[{}]\n(empty file)\n", empty.display()));
    }

    #[test]
    fn refuses_stdin_dash_and_missing_paths() {
        let err = run_read(
            ReadMode::Cat,
            &[PathBuf::from("-")],
            &ReadOptions::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("stdin"));

        let err = run_read(ReadMode::Nl, &[], &ReadOptions::default()).unwrap_err();
        assert!(err.to_string().contains("at least one file path"));
    }
}
