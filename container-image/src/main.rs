#![allow(clippy::disallowed_methods)]
//! Prints `base=<ref>` and `workspace=<ref>` for `$GITHUB_OUTPUT`: the
//! content-addressed GHCR references of the container images, the same ones
//! `may-agent`'s build script bakes into the binary.

use std::path::Path;

use may_container_image::image_ref;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.as_slice() != ["refs"] {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "usage: may-container-image refs",
        )
        .into());
    }
    let base = image_ref::base_image_ref(Path::new("container/base"))?;
    let distribution = may_container_image::combined_manifest_bytes()?;
    let lockfile = std::fs::read_to_string("Cargo.lock")?;
    let heel_lib_version = may_container_image::heel_version_in_lockfile(&lockfile)
        .ok_or("Cargo.lock must contain the heel package")?;
    let workspace = image_ref::workspace_image_ref(
        Path::new("container/workspace"),
        &base,
        &distribution,
        Path::new("."),
        &heel_lib_version,
    )?;
    println!("base={base}");
    println!("workspace={workspace}");
    Ok(())
}
