#![allow(clippy::disallowed_methods)]

use std::io::Write as _;
use std::process::{Command, Stdio};

use may_hashline::{hashed_line, line_hash_str};

fn search_bin() -> &'static str {
    env!("CARGO_BIN_EXE_search")
}

fn edit_bin() -> &'static str {
    env!("CARGO_BIN_EXE_edit")
}

fn run_search(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(search_bin())
        .current_dir(dir)
        .args(args)
        .output()
        .expect("run search")
}

fn run_edit(dir: &std::path::Path, stdin_text: &str) -> std::process::Output {
    let mut child = Command::new(edit_bin())
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn edit");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(stdin_text.as_bytes())
        .expect("write patch");
    child.wait_with_output().expect("wait edit")
}

fn anchor(line_no: usize, line: &str) -> String {
    format!("{line_no}#{}", line_hash_str(line))
}

#[test]
fn search_rg_supports_ripgrep_flags_bundling_and_per_line_hashes() {
    let dir = tempfile::tempdir().unwrap();
    let sample = "alpha = 1\nBeta = 2\ngamma_word = 3\ngamma = 4\ndelta = 5\n";
    std::fs::write(dir.path().join("app.py"), sample).unwrap();
    std::fs::write(dir.path().join("notes.txt"), "Beta note\n").unwrap();

    // `-n` and bundled `-ni` / `-nC1` / `-A1` / `-B1` must match `rg`.
    let out = run_search(dir.path(), &["rg", "-n", "Beta", "app.py"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        stdout,
        format!("[app.py]\n{}\n", hashed_line(2, "Beta = 2"))
    );

    let out = run_search(dir.path(), &["rg", "-ni", "-A1", "-B1", "beta", "app.py"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        stdout,
        format!(
            "[app.py]\n{}\n{}\n{}\n",
            hashed_line(1, "alpha = 1"),
            hashed_line(2, "Beta = 2"),
            hashed_line(3, "gamma_word = 3"),
        )
    );

    // `-w` whole word + `-t py` type filter + repeatable `-e`.
    let out = run_search(
        dir.path(),
        &["rg", "-nw", "-t", "py", "-e", "gamma", "-e", "alpha"],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        stdout,
        format!(
            "[app.py]\n{}\n--\n{}\n",
            hashed_line(1, "alpha = 1"),
            hashed_line(4, "gamma = 4"),
        )
    );

    // `-nC1` attached value + `-l` / `--files-without-match` / `-c` / `--files`.
    let out = run_search(dir.path(), &["rg", "-nC1", "delta", "app.py"]);
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!(
            "[app.py]\n{}\n{}\n",
            hashed_line(4, "gamma = 4"),
            hashed_line(5, "delta = 5"),
        )
    );

    let out = run_search(dir.path(), &["rg", "-c", "gamma", "app.py"]);
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "[app.py] (2 matches)\n"
    );

    let out = run_search(dir.path(), &["rg", "-c", "delta", "app.py"]);
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout), "[app.py] (1 match)\n");

    let out = run_search(dir.path(), &["rg", "-l", "delta", "app.py"]);
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout), "[app.py]\n");

    let out = run_search(
        dir.path(),
        &["rg", "--files-without-match", "delta", "notes.txt"],
    );
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout), "[notes.txt]\n");

    let out = run_search(dir.path(), &["rg", "--files", "-tpy"]);
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout), "[app.py]\n");
}

#[test]
fn search_sg_emits_section_and_per_line_hashes() {
    let dir = tempfile::tempdir().unwrap();
    let src = "fn alpha() {\n    let x = 1;\n}\n";
    std::fs::write(dir.path().join("lib.rs"), src).unwrap();

    let out = run_search(
        dir.path(),
        &["sg", "fn $NAME() { $$$ }", "--lang", "rust", "lib.rs"],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!(
            "[lib.rs]\n{}\n{}\n{}\n",
            hashed_line(1, "fn alpha() {"),
            hashed_line(2, "    let x = 1;"),
            hashed_line(3, "}"),
        )
    );
}

#[test]
fn search_sed_slices_ranges_and_refuses_mutations() {
    let dir = tempfile::tempdir().unwrap();
    let sample = "line1\ndef foo():\n    x = 1\n    return x\n\ndef bar():\n    return 2\n";
    std::fs::write(dir.path().join("mod.py"), sample).unwrap();

    let out = run_search(dir.path(), &["sed", "-n", "2,4p", "mod.py"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!(
            "[mod.py]\n{}\n{}\n{}\n",
            hashed_line(2, "def foo():"),
            hashed_line(3, "    x = 1"),
            hashed_line(4, "    return x"),
        )
    );

    // Omitted `-n` with pure print expression works identically.
    let out_no_n = run_search(dir.path(), &["sed", "2,4p", "mod.py"]);
    assert!(out_no_n.status.success());
    assert_eq!(out_no_n.stdout, out.stdout);

    let out = run_search(
        dir.path(),
        &["sed", "-n", "/def bar/,/return 2/p", "mod.py"],
    );
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!(
            "[mod.py]\n{}\n{}\n",
            hashed_line(6, "def bar():"),
            hashed_line(7, "    return 2"),
        )
    );

    let out = run_search(dir.path(), &["sed", "-i", "s/foo/baz/", "mod.py"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("use `edit`"));
}

#[test]
fn search_cat_head_tail_nl_and_invalid_flag_refusals() {
    let dir = tempfile::tempdir().unwrap();
    let sample = (1..=15)
        .map(|i| format!("row-{i}"))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(dir.path().join("nums.txt"), &sample).unwrap();
    std::fs::write(dir.path().join("empty.txt"), "").unwrap();

    // `search cat` with `-n`, `-b`, `--limit`, plus empty file.
    let out = run_search(dir.path(), &["cat", "-nb", "--limit", "3", "nums.txt"]);
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!(
            "[nums.txt]\n{}\n{}\n{}\n",
            hashed_line(1, "row-1"),
            hashed_line(2, "row-2"),
            hashed_line(3, "row-3"),
        )
    );

    let out = run_search(dir.path(), &["cat", "empty.txt"]);
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "[empty.txt]\n(empty file)\n"
    );

    // `search cat` refuses non-plain flags and `-`.
    for bad in [
        &["cat", "-A", "nums.txt"][..],
        &["cat", "-v", "nums.txt"],
        &["cat", "-E", "nums.txt"],
        &["cat", "-T", "nums.txt"],
        &["cat", "-"],
    ] {
        let res = run_search(dir.path(), bad);
        assert!(!res.status.success(), "expected refusal for {bad:?}");
    }

    // `search head` default (10), `-2`.
    let out = run_search(dir.path(), &["head", "nums.txt"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.starts_with(&format!("[nums.txt]\n{}\n", hashed_line(1, "row-1"))));
    assert!(stdout.ends_with(&format!("{}\n", hashed_line(10, "row-10"))));

    let out = run_search(dir.path(), &["head", "-2", "nums.txt"]);
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!(
            "[nums.txt]\n{}\n{}\n",
            hashed_line(1, "row-1"),
            hashed_line(2, "row-2"),
        )
    );

    for bad in [
        &["head", "-c", "10", "nums.txt"][..],
        &["head", "--bytes=5", "nums.txt"],
        &["head", "-"],
    ] {
        let res = run_search(dir.path(), bad);
        assert!(!res.status.success(), "expected refusal for {bad:?}");
    }

    // `search tail` `-n 3`, `-2`, `-n +13` with real 1-indexed line numbers.
    let out = run_search(dir.path(), &["tail", "-n", "3", "nums.txt"]);
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!(
            "[nums.txt]\n{}\n{}\n{}\n",
            hashed_line(13, "row-13"),
            hashed_line(14, "row-14"),
            hashed_line(15, "row-15"),
        )
    );

    let out = run_search(dir.path(), &["tail", "-2", "nums.txt"]);
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!(
            "[nums.txt]\n{}\n{}\n",
            hashed_line(14, "row-14"),
            hashed_line(15, "row-15"),
        )
    );

    let out = run_search(dir.path(), &["tail", "-n", "+13", "nums.txt"]);
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!(
            "[nums.txt]\n{}\n{}\n{}\n",
            hashed_line(13, "row-13"),
            hashed_line(14, "row-14"),
            hashed_line(15, "row-15"),
        )
    );

    for bad in [
        &["tail", "-f", "nums.txt"][..],
        &["tail", "-F", "nums.txt"],
        &["tail", "--follow", "nums.txt"],
        &["tail", "-c", "10", "nums.txt"],
        &["tail", "-"],
    ] {
        let res = run_search(dir.path(), bad);
        assert!(!res.status.success(), "expected refusal for {bad:?}");
    }

    // `search nl` accepts standard nl flags and emits `N#HH|content`.
    let out = run_search(
        dir.path(),
        &[
            "nl", "-ba", "-n", "ln", "-w", "4", "-s", ": ", "-v", "1", "-i", "1", "--limit", "2",
            "nums.txt",
        ],
    );
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!(
            "[nums.txt]\n{}\n{}\n",
            hashed_line(1, "row-1"),
            hashed_line(2, "row-2"),
        )
    );
    assert!(!run_search(dir.path(), &["nl", "-"]).status.success());
}

#[test]
fn edit_merges_sections_echoes_per_hunk_context_and_reports_mismatches() {
    let dir = tempfile::tempdir().unwrap();

    let mut lines = Vec::new();
    for i in 1..=200 {
        lines.push(format!("fn func_{i}() -> i32 {{ {i} }}"));
    }
    let text = format!("{}\n", lines.join("\n"));
    std::fs::write(dir.path().join("many.rs"), &text).unwrap();

    // 1. Repeated `[many.rs]` and `[./many.rs]` sections merge atomically, echo all
    // 3 disjoint hunks with `±2` hashed context separated by `--`, and report `(+2 lines)`.
    let multi_patch = format!(
        "[many.rs]\n\
         PUT {}:\n\
         +fn func_10() -> i32 {{ 100 }}\n\
         +// extra line 1\n\
         +// extra line 2\n\
         PUT {}:\n\
         +fn func_100() -> i32 {{ 1000 }}\n\
         [./many.rs]\n\
         PUT {}:\n\
         +fn func_190() -> i32 {{ 1900 }}\n",
        anchor(10, "fn func_10() -> i32 { 10 }"),
        anchor(100, "fn func_100() -> i32 { 100 }"),
        anchor(190, "fn func_190() -> i32 { 190 }"),
    );
    let mut child = Command::new(edit_bin())
        .current_dir(dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn edit");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(multi_patch.as_bytes())
        .unwrap();
    let ok = child.wait_with_output().unwrap();
    assert!(
        ok.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&ok.stderr)
    );

    let stdout = String::from_utf8_lossy(&ok.stdout);
    assert_eq!(stdout.matches("[many.rs]").count(), 1, "stdout:\n{stdout}");
    assert!(
        stdout.contains("[many.rs] (+2 lines)\n"),
        "stdout:\n{stdout}"
    );
    assert!(
        stdout.contains(&hashed_line(10, "fn func_10() -> i32 { 100 }")),
        "stdout:\n{stdout}"
    );
    assert!(
        stdout.contains(&hashed_line(102, "fn func_100() -> i32 { 1000 }")),
        "stdout:\n{stdout}"
    );
    assert!(
        stdout.contains(&hashed_line(192, "fn func_190() -> i32 { 1900 }")),
        "stdout:\n{stdout}"
    );
    assert_eq!(stdout.matches("\n--\n").count(), 2, "stdout:\n{stdout}");

    // 2. Stale line number (line 150 moved to line 152 after +2 insertion) refuses with
    // a crisp moved-line hint and live `N#HH|content` context.
    let stale_hash = line_hash_str("fn func_150() -> i32 { 150 }");
    let stale = run_edit(
        dir.path(),
        &format!("[many.rs]\nPUT 150#{stale_hash}:\n+fn func_150() -> i32 {{ 1500 }}\n"),
    );
    assert!(!stale.status.success());
    let stderr = String::from_utf8_lossy(&stale.stderr);
    assert!(
        stderr.contains(&format!("(hint: #{stale_hash} is at line 152)")),
        "stderr:\n{stderr}"
    );
    assert!(
        stderr.contains(&hashed_line(152, "fn func_150() -> i32 { 150 }")),
        "stderr:\n{stderr}"
    );

    // 3. Empty section probe refused cleanly.
    let empty = run_edit(dir.path(), "[many.rs]\n");
    assert!(!empty.status.success());
    assert!(
        String::from_utf8_lossy(&empty.stderr).contains("has no ops"),
        "stderr: {}",
        String::from_utf8_lossy(&empty.stderr)
    );
}
