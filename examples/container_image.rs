#![doc = "CI helper for the GHCR container images: print their content-addressed refs and stage the workspace build context."]

use std::path::Path;

use may_agent::container::image_ref;
use may_agent::container::stage::stage_workspace_context;

/// Byte for byte the manifest `embedded_distribution` loads; it is one of the
/// workspace reference's hash inputs.
const TOOL_DISTRIBUTION: &[u8] = include_bytes!("../src/tools/tool-distribution-0.12.0.json");

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
    let workspace = image_ref::workspace_image_ref(
        Path::new("container/workspace"),
        &base,
        TOOL_DISTRIBUTION,
        env!("MAY_HEEL_LIB_VERSION"),
    )?;
    println!("base={base}");
    println!("workspace={workspace}");
    Ok(())
}

/// Stages the generated workspace context entries for a Linux architecture:
/// the baseline tools under `<context>/tools/<cache key>/bin` and the managed
/// Linux `heel` binary at `<context>/heel`.
async fn stage(arch: &str, context: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let scratch = tempfile::tempdir()?;
    let staged = stage_workspace_context(arch, context, scratch.path()).await?;
    println!("tools={}", staged.display());
    println!("heel={}", context.join("heel").display());
    Ok(())
}
