//! Build script: bakes whether GitHub Actions compiles the binary into
//! `MAY_BUILT_BY_CI`, which `update::BuildKind` tells a CI build from a
//! local one. Actions sets `GITHUB_ACTIONS=true` in every step.

fn main() {
    println!("cargo:rerun-if-env-changed=GITHUB_ACTIONS");
    let built_by_ci = std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true");
    println!("cargo:rustc-env=MAY_BUILT_BY_CI={built_by_ci}");
}
