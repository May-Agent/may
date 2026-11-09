use std::path::PathBuf;

use super::*;

const DOCUMENT: &str = "# Plan\n\n## Roots\n- May-Agent/may#912\n\n## Doing\n- wire the command\n  detail one\n- write tests\n\n## Queued\n- document it\n\n## Notes\nfree text\n\n## Decisions\n> \"use files\"\n";

fn args(words: &[&str]) -> Vec<String> {
    words.iter().map(ToString::to_string).collect()
}

/// A tasks.md on disk holding `content`.
fn file(content: &str) -> (tempfile::TempDir, PathBuf) {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let path = directory.path().join("tasks.md");
    std::fs::write(&path, content).expect("the document is written");
    (directory, path)
}

fn run_on(path: &Path, words: &[&str]) -> Result<String, Error> {
    run(path, &args(words))
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).expect("the document exists")
}

#[test]
fn add_creates_the_file_and_the_section() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let path = directory.path().join("tasks.md");
    let out = run_on(
        &path,
        &[
            "add", "--title", "first", "--detail", "why", "--detail", "how",
        ],
    )
    .expect("add succeeds");
    assert_eq!(read(&path), "## Queued\n- first\n  why\n  how\n");
    assert!(out.starts_with("Added row 1."), "{out}");
    assert!(out.contains("1. first\n  why\n  how"), "{out}");
}

#[test]
fn add_goes_to_the_end_of_its_section_and_a_new_section_keeps_the_order() {
    let (_directory, path) = file(DOCUMENT);
    run_on(&path, &["add", "--title", "later", "--section", "queued"]).expect("add succeeds");
    run_on(&path, &["add", "--title", "now", "--section", "doing"]).expect("add succeeds");
    let content = read(&path);
    assert!(content.contains("- write tests\n- now\n\n## Queued\n- document it\n- later\n"));
    let (_directory, path) = file("## Notes\ntext\n");
    run_on(&path, &["add", "--title", "a", "--section", "doing"]).expect("add succeeds");
    assert_eq!(read(&path), "## Doing\n- a\n\n## Notes\ntext\n");
}

#[test]
fn start_moves_a_queued_row_to_doing_and_refuses_the_rest() {
    let (_directory, path) = file(DOCUMENT);
    let out = run_on(&path, &["start", "4"]).expect("start succeeds");
    assert!(out.starts_with("Row 4 is now under Doing."), "{out}");
    assert!(read(&path).contains("- write tests\n- document it\n\n## Queued\n\n## Notes"));
    let before = read(&path);
    assert!(matches!(
        run_on(&path, &["start", "2"]),
        Err(Error::Refused(Refusal::NotQueued { row: 2, .. }))
    ));
    assert_eq!(read(&path), before);
}

#[test]
fn finish_deletes_the_row_with_its_detail() {
    let (_directory, path) = file(DOCUMENT);
    run_on(&path, &["finish", "2"]).expect("finish succeeds");
    let content = read(&path);
    assert!(!content.contains("wire the command"));
    assert!(!content.contains("detail one"));
    assert!(content.contains("- write tests"));
}

#[test]
fn block_marks_a_row_and_unblock_removes_the_mark() {
    let (_directory, path) = file(DOCUMENT);
    run_on(&path, &["block", "3", "req-7", "answer"]).expect("block succeeds");
    let blocked = TasksFile::parse(&read(&path));
    assert_eq!(blocked.rows()[2].blocking_mark(), Some("req-7"));
    assert_eq!(blocked.rows()[2].title(), "write tests");
    run_on(&path, &["unblock", "3"]).expect("unblock succeeds");
    assert_eq!(
        TasksFile::parse(&read(&path)).rows()[2].blocking_mark(),
        None
    );
    assert!(matches!(
        run_on(&path, &["block", "1", "x"]),
        Err(Error::Refused(Refusal::NotWork { row: 1 }))
    ));
}

#[test]
fn edit_rewrites_the_title_and_the_detail_but_keeps_a_mark() {
    let (_directory, path) = file(DOCUMENT);
    run_on(&path, &["block", "2", "req-1"]).expect("block succeeds");
    run_on(
        &path,
        &["edit", "2", "--title", "wire it", "--detail", "new detail"],
    )
    .expect("edit succeeds");
    let file = TasksFile::parse(&read(&path));
    let row = &file.rows()[1];
    assert_eq!(row.title(), "wire it");
    assert_eq!(
        row.details().collect::<Vec<_>>(),
        vec!["new detail", "BLOCKING req-1"]
    );
    run_on(&path, &["edit", "2", "--clear-detail"]).expect("edit succeeds");
    let file = TasksFile::parse(&read(&path));
    assert_eq!(
        file.rows()[1].details().collect::<Vec<_>>(),
        vec!["BLOCKING req-1"]
    );
}

#[test]
fn note_and_decide_append_to_their_sections() {
    let (_directory, path) = file(DOCUMENT);
    run_on(&path, &["note", "a", "finding"]).expect("note succeeds");
    run_on(&path, &["decide", "> \"ship it\""]).expect("decide succeeds");
    let content = read(&path);
    assert!(content.contains("free text\na finding\n"));
    assert!(content.ends_with("> \"use files\"\n> \"ship it\"\n"));
    assert_eq!(TasksFile::parse(&content).violations(), &[]);
}

#[test]
fn list_prints_the_numbered_document_and_writes_nothing() {
    let (_directory, path) = file(DOCUMENT);
    let out = run_on(&path, &["list"]).expect("list succeeds");
    assert!(out.contains("1. May-Agent/may#912\n"), "{out}");
    assert!(out.contains("4. document it\n"), "{out}");
    assert_eq!(read(&path), DOCUMENT);
}

#[test]
fn a_malformed_edit_is_refused_and_writes_nothing() {
    let (_directory, path) = file(DOCUMENT);
    for refused in [
        vec!["add", "--title", "[ ] ticked"],
        vec!["add", "--title", "two\nlines"],
        vec!["add", "--title", " "],
        vec!["note", "## Doing"],
        vec!["note", "```"],
        vec!["finish", "9"],
        vec!["edit", "2", "--title", "[x] done"],
    ] {
        let result = run_on(&path, &refused);
        assert!(matches!(result, Err(Error::Refused(_))), "{refused:?}");
        assert_eq!(read(&path), DOCUMENT, "{refused:?}");
    }
    let Err(Error::Refused(Refusal::Malformed(problems))) = run_on(&path, &["note", "## Doing"])
    else {
        panic!("a repeated section heading in a note breaks the format");
    };
    assert!(problems[0].contains("repeats `## Doing`"), "{problems:?}");
}

#[test]
fn bad_arguments_print_usage() {
    let (_directory, path) = file(DOCUMENT);
    for words in [
        vec![],
        vec!["nope"],
        vec!["start"],
        vec!["start", "0"],
        vec!["add"],
        vec!["add", "--title"],
        vec!["add", "--title", "a", "--section", "notes"],
        vec!["list", "extra"],
    ] {
        let Err(error) = run_on(&path, &words) else {
            panic!("{words:?} names no operation");
        };
        assert!(matches!(error, Error::Usage(_)), "{words:?}");
        assert!(error.to_string().contains("Usage: tasks"), "{words:?}");
    }
}

#[test]
fn a_missing_directory_fails_instead_of_creating_one() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let path = directory.path().join("elsewhere").join("tasks.md");
    assert!(matches!(
        run_on(&path, &["add", "--title", "x"]),
        Err(Error::Io { .. })
    ));
    assert!(!path.parent().expect("has a parent").exists());
}

#[test]
fn the_document_the_model_reads_numbers_every_row() {
    let numbered = numbered(DOCUMENT);
    assert_eq!(
        numbered,
        "# Plan\n\n## Roots\n1. May-Agent/may#912\n\n## Doing\n2. wire the command\n  detail one\n3. write tests\n\n## Queued\n4. document it\n\n## Notes\nfree text\n\n## Decisions\n> \"use files\"\n"
    );
}
