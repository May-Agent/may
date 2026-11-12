use std::ffi::OsString;

use super::{in_place_requested, supports};

fn args(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

#[test]
fn supports_accepts_the_line_range_forms() {
    for forms in [
        &["-n", "2,3p", "a"][..],
        &["-n", "$p"],
        &["-n", "4,$p", "a"],
        &["-n", "2,3p;1p", "a"],
        &["-n", " 2 , 3 p ; 5p ", "a"],
        &["-n", "2p\n4p", "a"],
        &["-n", "4,2p", "a"],
        &["-ne", "3p", "a"],
        &["-n", "-e", "3p", "a"],
        &["--quiet", "-e", "1p;4,$p", "a", "b"],
        &["--version"],
    ] {
        assert!(supports(args(forms)).is_ok(), "{forms:?}");
    }
}

#[test]
fn supports_refuses_everything_else() {
    for forms in [
        &["-i", "2p", "a"][..],
        &["-n", "-i", "2p", "a"],
        &["-n", "-s", "2p", "a"],
        &["-n", "-E", "2p", "a"],
        &["-n", "--expression=2p", "a"],
        &["-n", "--in-place", "2p", "a"],
        &["-n", "--help"],
        &["s/a/b/", "a"],
        &["-n", "s/a/b/p", "a"],
        &["-n", "/a2/p", "a"],
        &["-n", "/a2/,/a4/p", "a"],
        &["-n", "1d", "a"],
        &["-n", "1!p", "a"],
        &["-n", "2~2p", "a"],
        &["-n", "0p", "a"],
        &["-n", "0,3p", "a"],
        &["-n", "1p;s/a/b/", "a"],
        &["-n", "1,2,3p", "a"],
        &["-n", "$,3p", "a"],
        &["-n", "1pp", "a"],
        &["-n", "1p p", "a"],
        &["-n", "1 2p", "a"],
        &["-n", "{1p}", "a"],
        &["-n", "p", "a"],
        &["-n", "1", "a"],
        &["-n", "", "a"],
        &["-n", ";", "a"],
        &["-n"],
        &[],
        &["-n", "-e", "1p", "-e", "2p", "a"],
        &["-n", "-e"],
        &["2p", "a"],
        &["--version", "a"],
    ] {
        assert!(supports(args(forms)).is_err(), "{forms:?}");
    }
}

#[test]
fn in_place_is_found_in_every_short_form_clap_cannot_parse() {
    for forms in [
        &["-i.bak", "s/a/b/", "f"][..],
        &["-ni.bak", "s/a/b/", "f"],
        &["-n", "-i", "s/a/b/", "f"],
        &["-ni", "s/a/b/", "f"],
        &["-n", "-e", "p", "-i.b", "f"],
    ] {
        assert!(in_place_requested(&args(forms)), "{forms:?}");
    }
}

#[test]
fn in_place_is_not_found_in_a_value_or_after_the_options() {
    for forms in [
        &["-e", "s/i/x/", "f"][..],
        &["-es/i/x/", "f"],
        &["-n", "-e", "i\\foo", "f"],
        &["-f", "-i", "f"],
        &["-l", "-i", "f"],
        &["--expression", "-i", "f"],
        &["--expression=-i", "f"],
        &["-n", "--", "-i"],
        &["-", "p"],
        &["-n", "s/i/x/", "f"],
    ] {
        assert!(!in_place_requested(&args(forms)), "{forms:?}");
    }
}
