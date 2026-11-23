#![allow(clippy::disallowed_methods)]
//! Build script: bakes the pinned `heel` library version and the
//! content-addressed GHCR image references into the crate, as
//! `MAY_HEEL_LIB_VERSION`, `MAY_BASE_IMAGE` and `MAY_WORKSPACE_IMAGE`.
//!
//! Paths are relative to `agent-core/`; `..` is the repository root.

use std::fs;
use std::path::{Path, PathBuf};

use may_container_image::{combined_manifest_bytes, heel_version_in_lockfile, image_ref};

/// The manifest files the workspace image's tag hashes, through
/// `manifest::combined_manifest_bytes`.
const TOOL_DISTRIBUTION_MANIFESTS: [&str; 3] = [
    "tool-dist/src/third-party-tools.json",
    "tool-dist/src/tool-artifacts.json",
    "tool-dist/src/host-programs.json",
];

/// The repository root: `agent-core/`'s parent.
fn repository() -> &'static Path {
    Path::new("..")
}

fn main() {
    println!("cargo:rerun-if-changed=../Cargo.lock");

    let heel_lib_version = emit_heel_lib_version();
    emit_image_refs(&heel_lib_version);
}

/// The version of the `heel` library `Cargo.lock` pins, which the
/// distribution's `heel` binary must share; the workspace image is named by
/// it and a test checks the binary's pin against it.
fn emit_heel_lib_version() -> String {
    let lockfile = fs::read_to_string("../Cargo.lock").expect("Cargo.lock must be readable");
    let version =
        heel_version_in_lockfile(&lockfile).expect("Cargo.lock must contain the heel package");
    println!("cargo:rustc-env=MAY_HEEL_LIB_VERSION={version}");
    version
}

/// The GHCR references CI publishes both container images under, named by the
/// content hash of everything that goes into them, so a published tag is
/// never overwritten and no pin is updated by hand.
fn emit_image_refs(heel_lib_version: &str) {
    let repository = repository();
    let base_dir = repository.join("container/base");
    let workspace_dir = repository.join("container/workspace");
    let manifest =
        combined_manifest_bytes().expect("tool distribution manifests must be valid JSON");
    let base_ref = image_ref::base_image_ref(&base_dir).expect("base context must be readable");
    let workspace_ref = image_ref::workspace_image_ref(
        &workspace_dir,
        &base_ref,
        &manifest,
        repository,
        heel_lib_version,
    )
    .expect("workspace context and tasks sources must be readable");
    println!("cargo:rustc-env=MAY_BASE_IMAGE={base_ref}");
    println!("cargo:rustc-env=MAY_WORKSPACE_IMAGE={workspace_ref}");

    let mut inputs: Vec<PathBuf> = TOOL_DISTRIBUTION_MANIFESTS
        .into_iter()
        .map(|relative| repository.join(relative))
        .collect();
    inputs.push(workspace_dir.clone());
    inputs.extend(
        image_ref::tasks_sources(repository)
            .expect("tasks crate must be readable")
            .into_iter()
            .map(|relative| repository.join(relative)),
    );
    inputs.push(repository.join(image_ref::PYTHON_SHIM_TEMPLATE));
    inputs.extend(
        image_ref::workspace_hash_inputs(&workspace_dir)
            .expect("workspace context must be readable")
            .into_iter()
            .map(|relative| workspace_dir.join(relative)),
    );
    watch_context_inputs(&base_dir);
    for input in inputs {
        println!("cargo:rerun-if-changed={}", input.display());
    }
}

/// Emit `rerun-if-changed` for every file a context hash reads.
fn watch_context_inputs(context_dir: &Path) {
    let mut stack = vec![context_dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                println!("cargo:rerun-if-changed={}", path.display());
            }
        }
    }
}
