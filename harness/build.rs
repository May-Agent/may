#![allow(clippy::disallowed_methods)]
//! Emits `MAY_GIT_SHA`, the commit `update` reports a build as coming from.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    Command::new("git")
        .args(args)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn main() {
    for name in ["HEAD", "logs/HEAD"] {
        if let Some(path) = git(&["rev-parse", "--git-path", name]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    let sha = git(&["rev-parse", "HEAD"]).unwrap_or_default();
    println!("cargo:rustc-env=MAY_GIT_SHA={sha}");
}
