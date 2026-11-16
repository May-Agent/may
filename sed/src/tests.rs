use std::ffi::OsString;

use super::in_place_requested;

fn args(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
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
