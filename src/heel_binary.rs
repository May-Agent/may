use std::path::{Path, PathBuf};
use std::process::Command;

use cargo_metadata::{Metadata, MetadataCommand, Package};

const HEEL_PACKAGE_NAME: &str = "heel";

#[derive(Debug)]
struct HeelPaths {
    manifest_dir: PathBuf,
    target_dir: PathBuf,
}

pub fn configure_heel_binary() {
    if std::env::var_os("HEEL_BIN").is_some() {
        return;
    }

    let binary_name = heel_binary_name();
    if search_path_for_binary(&binary_name).is_some() {
        return;
    }

    match resolve_heel_binary() {
        Ok(path) => {
            // SAFETY: this runs during process startup before May creates background tasks.
            unsafe { std::env::set_var("HEEL_BIN", &path) };
            tracing::info!(heel_bin = %path.display(), "configured HEEL_BIN for May runtime");
        }
        Err(error) => {
            tracing::warn!(error = %error, "failed to preconfigure HEEL_BIN");
        }
    }
}

fn heel_binary_name() -> String {
    format!("heel{}", std::env::consts::EXE_SUFFIX)
}

fn resolve_heel_binary() -> anyhow::Result<PathBuf> {
    let paths = heel_paths()?;
    let binary_path = paths.target_dir.join("debug").join(heel_binary_name());
    if binary_path.is_file() {
        return Ok(binary_path);
    }

    build_heel_binary(&paths.manifest_dir)?;
    anyhow::ensure!(
        binary_path.is_file(),
        "heel build succeeded but binary is missing at {}",
        binary_path.display()
    );
    Ok(binary_path)
}

fn heel_paths() -> anyhow::Result<HeelPaths> {
    let metadata = MetadataCommand::new()
        .manifest_path(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"))
        .exec()?;
    let package = find_heel_package(&metadata)?;
    let manifest_dir = package_manifest_dir(&package)?;
    let target_dir = package_target_dir(&package)?;
    Ok(HeelPaths {
        manifest_dir,
        target_dir,
    })
}

fn find_heel_package(metadata: &Metadata) -> anyhow::Result<Package> {
    metadata
        .packages
        .iter()
        .find(|package| package.name.as_ref() == HEEL_PACKAGE_NAME)
        .cloned()
        .ok_or_else(|| {
            anyhow::anyhow!("cargo metadata did not include a '{HEEL_PACKAGE_NAME}' package")
        })
}

fn package_manifest_dir(package: &Package) -> anyhow::Result<PathBuf> {
    package
        .manifest_path
        .parent()
        .map(|path| path.as_std_path().to_path_buf())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "package manifest path has no parent: {}",
                package.manifest_path
            )
        })
}

fn package_target_dir(package: &Package) -> anyhow::Result<PathBuf> {
    let metadata = MetadataCommand::new()
        .manifest_path(package.manifest_path.as_std_path())
        .no_deps()
        .exec()?;
    Ok(metadata.target_directory.as_std_path().to_path_buf())
}

fn build_heel_binary(manifest_dir: &Path) -> anyhow::Result<()> {
    let status = Command::new("cargo")
        .current_dir(manifest_dir)
        .arg("build")
        .arg("--bin")
        .arg(HEEL_PACKAGE_NAME)
        .status()?;
    anyhow::ensure!(
        status.success(),
        "cargo build --bin heel exited with status {status}"
    );
    Ok(())
}

fn search_path_for_binary(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}
