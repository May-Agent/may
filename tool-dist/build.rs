#![allow(clippy::disallowed_methods)]
//! Build script: bakes the `heel` library version `Cargo.lock` pins into
//! `MAY_HEEL_LIB_VERSION`, which a test checks the distribution's `heel`
//! pins against.

use std::fs;

fn main() {
    println!("cargo:rerun-if-changed=../Cargo.lock");
    let lockfile = fs::read_to_string("../Cargo.lock").expect("Cargo.lock must be readable");
    let version = may_container_image::heel_version_in_lockfile(&lockfile)
        .expect("Cargo.lock must contain the heel package");
    println!("cargo:rustc-env=MAY_HEEL_LIB_VERSION={version}");
}
