#![doc = "CI helper for the GHCR container images: print their content-addressed refs and stage the workspace build context."]

use std::path::{Path, PathBuf};

use may_agent::container::image_ref;
use may_agent::container::stage::stage_workspace_context;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    match args.as_slice() {
        [command] if command == "refs" => refs(),
        [command, arch, context] if command == "stage" => stage(arch, Path::new(context)).await,
        _ => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "usage: container_image refs | container_image stage <amd64|arm64> <context-dir>",
        )
        .into()),
    }
}

/// Prints `base=<ref>` and `workspace=<ref>` for `$GITHUB_OUTPUT`.
fn refs() -> Result<(), Box<dyn std::error::Error>> {
    let base = image_ref::base_image_ref(Path::new("container/base"))?;
    // The manifest `embedded_distribution` loads, one of the reference's hash inputs.
    let distribution = may_agent::tools::manifest::combined_manifest_bytes()?;
    let workspace = image_ref::workspace_image_ref(
        Path::new("container/workspace"),
        &base,
        &distribution,
        Path::new("."),
        env!("MAY_HEEL_LIB_VERSION"),
    )?;
    println!("base={base}");
    println!("workspace={workspace}");
    Ok(())
}

/// Builds the `tasks` binary for the host architecture, which is the image's.
fn build_tasks() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = std::process::Command::new(cargo)
        .args(["build", "--locked", "-p", "may-tasks", "--bin", "tasks"])
        .status()?;
    if !status.success() {
        return Err(format!("building the tasks binary failed: {status}").into());
    }
    // `<target>/<profile>/examples/container_image` sits beside `<target>/<profile>/tasks`.
    let example = std::env::current_exe()?;
    let profile = example
        .ancestors()
        .nth(2)
        .ok_or("the example is not under <target>/<profile>/examples")?;
    Ok(profile.join("tasks"))
}

/// Stages the generated workspace context entries for a Linux architecture:
/// the baseline tools under `<context>/tools/<cache key>/bin`, the managed
/// Linux `heel` binary at `<context>/heel` and the `tasks` command at
/// `<context>/tasks`.
async fn stage(arch: &str, context: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let scratch = tempfile::tempdir()?;
    let tasks = build_tasks()?;
    let staged = stage_workspace_context(arch, context, scratch.path(), &tasks).await?;
    println!("tools={}", staged.display());
    println!("heel={}", context.join("heel").display());
    println!("tasks={}", context.join("tasks").display());
    Ok(())
}
