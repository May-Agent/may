//! The executable as the sandbox runs it.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

fn aither_sed(directory: &Path, args: &[&str], stdin: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_aither-sed"))
        .args(args)
        .current_dir(directory)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn aither-sed");
    // A refused script exits without reading its input, which then meets a
    // closed pipe.
    match child
        .stdin
        .take()
        .expect("stdin")
        .write_all(stdin.as_bytes())
    {
        Err(error) if error.kind() != std::io::ErrorKind::BrokenPipe => {
            panic!("write stdin: {error}")
        }
        _ => {}
    }
    child.wait_with_output().expect("wait")
}

/// Runs `args` on `stdin` and returns the standard output of a run that
/// succeeded without a diagnostic.
fn printed(args: &[&str], stdin: &str) -> String {
    let directory = tempfile::tempdir().expect("tempdir");
    let output = aither_sed(directory.path(), args, stdin);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{args:?}: {stderr}");
    assert_eq!(stderr, "", "{args:?}");
    String::from_utf8(output.stdout).expect("stdout")
}

#[test]
fn version_is_the_name_and_the_package_version() {
    let directory = tempfile::tempdir().expect("tempdir");
    let output = aither_sed(directory.path(), &["--version"], "");
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        concat!("aither-sed ", env!("CARGO_PKG_VERSION"), "\n")
    );
}

#[test]
fn line_ranges_print() {
    assert_eq!(printed(&["-n", "2,3p"], "x\ny\nz\nw\n"), "y\nz\n");
    assert_eq!(printed(&["-n", "$p"], "x\ny\nz\n"), "z\n");
}

/// The expected outputs are what GNU sed prints for the same script and input.
#[test]
fn regex_addresses_and_ranges_print_as_gnu_sed_does() {
    for (args, input, expected) in [
        (
            &["-n", "/foo/,/bar/p"][..],
            "x\nfoo\ny\nbar\nz\n",
            "foo\ny\nbar\n",
        ),
        (&["-n", "/foo/,+1p"], "x\nfoo\ny\nz\nfoo\n", "foo\ny\nfoo\n"),
        (&["-n", "2,/c/p"], "a\nb\nc\nd\n", "b\nc\n"),
        (&["-n", "/b/p"], "a\nb\nc\n", "b\n"),
        (&["-n", "/b/,$p"], "a\nb\nc\n", "b\nc\n"),
    ] {
        assert_eq!(printed(args, input), expected, "{args:?}");
    }
}

#[test]
fn the_commands_that_write_nothing_run() {
    for (args, input, expected) in [
        (&["s/a/b/"][..], "abc\naaa\n", "bbc\nbaa\n"),
        (&["s/a/b/g"], "abc\naaa\n", "bbc\nbbb\n"),
        (&["-E", "s/(a+)b/[\\1]/"], "aab\nb\n", "[aa]\nb\n"),
        (&["2d"], "1\n2\n3\n", "1\n3\n"),
        (&["2q"], "1\n2\n3\n", "1\n2\n"),
        (&["y/abc/xyz/"], "aabbcc\n", "xxyyzz\n"),
        (&["="], "a\nb\n", "1\na\n2\nb\n"),
        (&["-e", "s/a/b/", "-e", "s/b/c/"], "a\n", "c\n"),
        (&["s/語/日本/"], "漢語\n", "漢日本\n"),
    ] {
        assert_eq!(printed(args, input), expected, "{args:?}");
    }
}

#[test]
fn reads_the_files_it_is_given() {
    let directory = tempfile::tempdir().expect("tempdir");
    std::fs::write(directory.path().join("a"), "a1\na2\n").expect("write a");
    let output = aither_sed(directory.path(), &["-n", "/a2/p", "a"], "");
    assert!(output.status.success());
    assert_eq!(output.stdout, b"a2\n");
}

/// Every write form fails with the `apply_patch` message before a file is
/// opened or created.
#[test]
fn every_write_form_is_refused_and_touches_no_file() {
    for args in [
        &["w out"][..],
        &["W out"],
        &["r in"],
        &["R in"],
        &["s/a/b/w out"],
        &["s/a/b/e"],
        &["e"],
        &["1e echo hi"],
        &["-e", "p", "-e", "w out"],
        &["-n", "/a/{p;w out\n}"],
        &["-i", "s/a/b/", "target"],
        &["--in-place", "s/a/b/", "target"],
        &["--in-pl", "s/a/b/", "target"],
        &["-ni", "s/a/b/", "target"],
        &["-i.bak", "s/a/b/", "target"],
        &["-ni.bak", "s/a/b/", "target"],
        &["-n", "-i", "s/a/b/", "target"],
    ] {
        let directory = tempfile::tempdir().expect("tempdir");
        std::fs::write(directory.path().join("target"), "a\n").expect("write target");
        std::fs::write(directory.path().join("in"), "x\n").expect("write in");
        let output = aither_sed(directory.path(), args, "a\n");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{args:?}: {stderr}");
        assert_eq!(output.stdout, b"", "{args:?}");
        assert!(stderr.contains("`apply_patch`"), "{args:?}: {stderr}");
        assert!(
            stderr.starts_with("Refused before running:"),
            "{args:?}: {stderr}"
        );
        assert!(!directory.path().join("out").exists(), "{args:?}");
        assert_eq!(
            std::fs::read_to_string(directory.path().join("target")).expect("read target"),
            "a\n",
            "{args:?}"
        );
    }
}

/// An `i` in a script or its value is not the in-place flag.
#[test]
fn an_i_inside_a_script_is_not_in_place_editing() {
    let directory = tempfile::tempdir().expect("tempdir");
    std::fs::write(directory.path().join("target"), "hi\n").expect("write target");
    for args in [
        &["-e", "s/i/x/", "target"][..],
        &["-es/i/x/", "target"],
        &["-n", "-e", "s/i/x/p", "target"],
    ] {
        let output = aither_sed(directory.path(), args, "");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{args:?}: {stderr}");
        assert_eq!(output.stdout, b"hx\n", "{args:?}");
    }
    assert_eq!(
        std::fs::read_to_string(directory.path().join("target")).expect("read target"),
        "hi\n"
    );
}

#[test]
fn a_syntax_error_is_not_a_write_refusal() {
    let directory = tempfile::tempdir().expect("tempdir");
    let output = aither_sed(directory.path(), &["s/a/b"], "a\n");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(!stderr.contains("apply_patch"), "{stderr}");
}
