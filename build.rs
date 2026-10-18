//! Build script: builds the web UI bundle with `bun` when the `ui` feature is
//! enabled so its `dist/` assets can be embedded in the binary.

use std::fs;
use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=MAY_SKIP_WEB_UI_BUILD");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_UI");
    println!("cargo:rerun-if-changed=Cargo.lock");

    emit_heel_lib_version();
    watch_web_ui_inputs();

    let enable_ui = std::env::var_os("CARGO_FEATURE_UI").is_some();
    if !enable_ui {
        return;
    }

    if std::env::var("MAY_SKIP_WEB_UI_BUILD").as_deref() == Ok("1") {
        return;
    }

    assert!(
        Path::new("web-ui").exists(),
        "web-ui directory not found; cannot embed frontend assets"
    );

    let status = Command::new("bun")
        .arg("run")
        .arg("build")
        .current_dir("web-ui")
        .status()
        .unwrap_or_else(|err| panic!("failed to run bun build for web-ui: {err}"));

    assert!(
        status.success(),
        "bun run build failed in web-ui; cannot embed dist assets"
    );

    assert!(
        Path::new("web-ui/dist/index.html").exists(),
        "web-ui build did not produce dist/index.html"
    );
}

/// The `heel` binary that executes sandbox IPC commands is provisioned
/// separately from the `heel` library that generates those commands' shims,
/// and the two must share a version: the library is pinned by `Cargo.lock`,
/// so the managed-binary download requests the matching release tag.
fn emit_heel_lib_version() {
    let lockfile = fs::read_to_string("Cargo.lock").expect("Cargo.lock must be readable");
    let version =
        heel_version_in_lockfile(&lockfile).expect("Cargo.lock must contain the heel package");
    println!("cargo:rustc-env=MAY_HEEL_LIB_VERSION={version}");
}

/// Extract the `version` field of the `heel` package entry.
fn heel_version_in_lockfile(lockfile: &str) -> Option<String> {
    let mut in_heel_package = false;
    for line in lockfile.lines() {
        if line.starts_with("[[") {
            in_heel_package = false;
        }
        if line == "name = \"heel\"" {
            in_heel_package = true;
        } else if in_heel_package
            && let Some(version) = line
                .strip_prefix("version = \"")
                .and_then(|rest| rest.strip_suffix('"'))
        {
            return Some(version.to_string());
        }
    }
    None
}

fn watch_web_ui_inputs() {
    let web_ui_dir = Path::new("web-ui");
    if !web_ui_dir.exists() {
        return;
    }

    watch_dir_recursive(web_ui_dir);
}

fn watch_dir_recursive(dir: &Path) {
    if should_skip_watch_path(dir) {
        return;
    }

    if let Some(s) = dir.to_str() {
        println!("cargo:rerun-if-changed={s}");
    }

    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if should_skip_watch_path(&path) {
            continue;
        }

        if path.is_dir() {
            watch_dir_recursive(&path);
        } else if let Some(s) = path.to_str() {
            println!("cargo:rerun-if-changed={s}");
        }
    }
}

fn should_skip_watch_path(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(|name| name.to_str()),
        Some("dist" | "node_modules" | ".git")
    )
}
