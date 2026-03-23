use std::fs;
use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=MAY_SKIP_WEB_UI_BUILD");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_UI");

    watch_path("web-ui/src");
    watch_path("web-ui/public");
    watch_path("web-ui/index.html");
    watch_path("web-ui/package.json");
    watch_path("web-ui/bun.lock");
    watch_path("web-ui/tsconfig.json");
    watch_path("web-ui/vite.config.ts");

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

fn watch_path(path: &str) {
    let p = Path::new(path);
    if !p.exists() {
        return;
    }

    if p.is_file() {
        println!("cargo:rerun-if-changed={path}");
        return;
    }

    watch_dir_recursive(p);
}

fn watch_dir_recursive(dir: &Path) {
    if let Some(s) = dir.to_str() {
        println!("cargo:rerun-if-changed={s}");
    }

    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            watch_dir_recursive(&path);
        } else if let Some(s) = path.to_str() {
            println!("cargo:rerun-if-changed={s}");
        }
    }
}
