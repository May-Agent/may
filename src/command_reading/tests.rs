use super::*;

fn intents(script: &str) -> Vec<Intent> {
    CommandReading::of(script)
        .steps
        .into_iter()
        .map(|step| step.intent)
        .collect()
}

fn unknown(source: &str) -> Intent {
    Intent::Unknown {
        source: source.to_owned(),
    }
}

fn read(path: &str, range: ReadRange) -> Intent {
    Intent::Read {
        path: path.to_owned(),
        range,
    }
}

#[test]
fn an_apply_patch_heredoc_reads_as_an_edit_before_it_runs() {
    let script = "apply_patch <<'PATCH'\n*** Begin Patch\n*** Update File: src/a.rs\n@@\n-old\n+new\n*** Add File: b.txt\n+x\n*** End Patch\nPATCH\n";
    let reading = CommandReading::of(script);
    assert_eq!(reading.steps.len(), 1);
    let Intent::Edit { files } = &reading.steps[0].intent else {
        panic!("edit expected, got {:?}", reading.steps[0].intent);
    };
    assert_eq!(files.len(), 2);
    assert_eq!(files[0].path, "src/a.rs");
    assert_eq!(files[1].path, "b.txt");
    assert_eq!(reading.steps[0].line, "Edit 2 files");
}

#[test]
fn a_patch_piped_from_cat_reads_as_one_edit() {
    let script = "cat <<'EOF' | apply_patch\n*** Begin Patch\n*** Delete File: gone.txt\n*** End Patch\nEOF\n";
    let reading = CommandReading::of(script);
    assert_eq!(reading.steps.len(), 1);
    assert!(matches!(&reading.steps[0].intent, Intent::Edit { files } if files.len() == 1));
    assert_eq!(reading.steps[0].line, "Edit gone.txt");
}

#[test]
fn an_unquoted_heredoc_with_expansion_is_unknown() {
    let script =
        "apply_patch <<EOF\n*** Begin Patch\n*** Add File: a\n+$HOME\n*** End Patch\nEOF\n";
    assert!(matches!(intents(script)[..], [Intent::Unknown { .. }]));
}

#[test]
fn a_malformed_patch_is_unknown() {
    let script = "apply_patch <<'EOF'\nnot a patch\nEOF\n";
    assert!(matches!(intents(script)[..], [Intent::Unknown { .. }]));
}

#[test]
fn reads_are_recognised_with_their_range() {
    assert_eq!(intents("cat a.rs"), [read("a.rs", ReadRange::Whole)]);
    assert_eq!(
        intents("cat a.rs b.rs"),
        [
            read("a.rs", ReadRange::Whole),
            read("b.rs", ReadRange::Whole)
        ]
    );
    assert_eq!(
        intents("head -n 20 src/lib.rs"),
        [read("src/lib.rs", ReadRange::Head { lines: 20 })]
    );
    assert_eq!(
        intents("head -5 a"),
        [read("a", ReadRange::Head { lines: 5 })]
    );
    assert_eq!(
        intents("tail -n20 log.txt"),
        [read("log.txt", ReadRange::Tail { lines: 20 })]
    );
    assert_eq!(
        intents("sed -n '10,40p' src/x.rs"),
        [read("src/x.rs", ReadRange::Lines { start: 10, end: 40 })]
    );
    assert_eq!(intents("nl a.txt"), [read("a.txt", ReadRange::Whole)]);
    assert_eq!(
        CommandReading::of("sed -n '10,40p' src/x.rs").steps[0].line,
        "Read lines 10 to 40 of src/x.rs"
    );
}

#[test]
fn searches_lists_and_builds_are_recognised() {
    assert_eq!(
        intents("rg -n \"fn main\" src"),
        [Intent::Search {
            tool: SearchTool::Rg,
            pattern: Some("fn main".into()),
            paths: vec!["src".into()],
        }]
    );
    assert_eq!(
        intents("grep -rn TODO ."),
        [Intent::Search {
            tool: SearchTool::Grep,
            pattern: Some("TODO".into()),
            paths: vec![".".into()],
        }]
    );
    assert_eq!(
        intents("fd -e rs main src"),
        [Intent::Search {
            tool: SearchTool::Fd,
            pattern: Some("main".into()),
            paths: vec!["src".into()],
        }]
    );
    assert_eq!(
        intents("find . -name '*.rs' -type f"),
        [Intent::Search {
            tool: SearchTool::Find,
            pattern: Some("*.rs".into()),
            paths: vec![".".into()],
        }]
    );
    assert_eq!(
        intents("ls -la src"),
        [Intent::List {
            tool: ListTool::Ls,
            paths: vec!["src".into()],
        }]
    );
    assert_eq!(
        intents("tree -L 2"),
        [Intent::List {
            tool: ListTool::Tree,
            paths: vec![],
        }]
    );
    assert_eq!(
        intents("git -C repo status --short"),
        [Intent::Git {
            subcommand: Some("status".into())
        }]
    );
    assert_eq!(
        intents("cargo test -p may"),
        [Intent::Build {
            tool: BuildTool::Cargo,
            subcommand: Some("test".into()),
            target: None,
        }]
    );
    assert_eq!(
        intents("bun run lint"),
        [Intent::Build {
            tool: BuildTool::Bun,
            subcommand: Some("run".into()),
            target: Some("lint".into()),
        }]
    );
    assert_eq!(
        CommandReading::of("cargo test").steps[0].line,
        "Run cargo test"
    );
    assert_eq!(
        CommandReading::of("rg foo src tests").steps[0].line,
        "Search for \"foo\" in src, tests"
    );
}

#[test]
fn a_pipeline_and_a_list_keep_their_parts_and_links() {
    let reading = CommandReading::of("rg -n foo src | head -20 && cargo test; ls");
    let links: Vec<Link> = reading.steps.iter().map(|step| step.link).collect();
    assert_eq!(links, [Link::Start, Link::Pipe, Link::And, Link::Then]);
    // `head` reads its input from the pipe, which May cannot see: unknown.
    assert_eq!(reading.steps[1].intent, unknown("head -20"));
    assert!(matches!(reading.steps[0].intent, Intent::Search { .. }));
    assert!(matches!(reading.steps[3].intent, Intent::List { .. }));
    assert!(reading.recognises_any());
}

#[test]
fn an_or_list_is_linked_with_or() {
    let reading = CommandReading::of("cat a || cat b");
    assert_eq!(reading.steps[1].link, Link::Or);
}

#[test]
fn what_cannot_be_resolved_is_unknown_never_a_wrong_path() {
    for script in [
        "cat $FILE",
        "cat \"$HOME/a\"",
        "cat $(pwd)/a",
        "cat `pwd`/a",
        "cat src/*.rs",
        "cat ~/notes",
        "cat {a,b}",
        "cat $'a\\tb'",
        "FOO=1 cat a",
        "cat a > out",
        "rg $PATTERN src",
        "head -n $N a",
        "find . -delete",
        "find . -name a -exec rm {} ;",
        "sed -i s/a/b/ f",
        "sed -n 5,1p f",
    ] {
        let steps = intents(script);
        assert!(
            steps.iter().all(|i| matches!(i, Intent::Unknown { .. })),
            "{script} read as {steps:?}"
        );
    }
}

#[test]
fn quoting_is_removed_from_literals() {
    assert_eq!(
        intents("cat 'my file.txt' \"other file\" a\\ b"),
        [
            read("my file.txt", ReadRange::Whole),
            read("other file", ReadRange::Whole),
            read("a b", ReadRange::Whole),
        ]
    );
    // A glob in quotes is text, not an expansion.
    assert_eq!(intents("cat 'a*'"), [read("a*", ReadRange::Whole)]);
}

#[test]
fn output_redirects_to_nowhere_do_not_hide_a_command() {
    assert_eq!(
        intents("cargo test 2>&1 >/dev/null"),
        [Intent::Build {
            tool: BuildTool::Cargo,
            subcommand: Some("test".into()),
            target: None,
        }]
    );
}

#[test]
fn compound_commands_and_background_jobs_are_unknown() {
    assert!(matches!(
        intents("for f in a b; do cat $f; done")[..],
        [Intent::Unknown { .. }]
    ));
    assert!(matches!(intents("sleep 1 &")[..], [Intent::Unknown { .. }]));
    assert!(matches!(intents("echo hi")[..], [Intent::Unknown { .. }]));
}

#[test]
fn a_script_that_does_not_parse_is_one_unknown_step() {
    let reading = CommandReading::of("cat 'unterminated");
    assert_eq!(reading.steps.len(), 1);
    assert_eq!(reading.steps[0].intent, unknown("cat 'unterminated"));
    assert!(!reading.recognises_any());
}

#[test]
fn the_reading_serializes_for_the_wire() {
    let reading = CommandReading::of("cat a.rs");
    let json = serde_json::to_value(&reading).expect("serializes");
    assert_eq!(
        json["steps"][0]["intent"],
        serde_json::json!({"kind": "read", "path": "a.rs", "range": {"kind": "whole"}})
    );
    assert_eq!(json["steps"][0]["link"], "start");
    assert_eq!(json["steps"][0]["line"], "Read a.rs");
    let back: CommandReading = serde_json::from_value(json).expect("deserializes");
    assert_eq!(back, reading);
}
