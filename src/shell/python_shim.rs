//! The `python3` and `python` commands of a terminal: the interpreter of the
//! project environment that holds the working directory, else the managed
//! `python3`.
//!
//! The managed `python3` is a verified link inside the prepared tools
//! directory, so the commands live in a directory of their own, which leads
//! the session's `PATH`. They resolve at run time, from the directory the
//! command runs in.

use std::path::{Path, PathBuf};

/// The shim both commands are.
const PYTHON_SHIM: &str = include_str!("../../templates/python_shim");

/// The commands the shim is installed as.
const COMMANDS: [&str; 2] = ["python3", "python"];

/// The directory of a session's shims.
pub(super) fn shim_directory(working_dir: &Path) -> PathBuf {
    working_dir.join(".cache").join("shims")
}

/// Installs the shims into `directory`, rewriting one whose content differs
/// from the template.
#[cfg(unix)]
pub(super) async fn install(directory: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    async_fs::create_dir_all(directory).await?;
    for command in COMMANDS {
        let shim = directory.join(command);
        let current = match async_fs::read(&shim).await {
            Ok(current) => Some(current),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        if current.as_deref() == Some(PYTHON_SHIM.as_bytes()) {
            continue;
        }
        let staged = directory.join(format!(".{command}.{}", std::process::id()));
        async_fs::write(&staged, PYTHON_SHIM).await?;
        async_fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755)).await?;
        async_fs::rename(&staged, &shim).await?;
    }
    Ok(())
}

/// Windows commands run no `sh`: no shim.
#[cfg(not(unix))]
pub(super) async fn install(_directory: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use std::path::Path;
    use std::process::Command;

    use super::*;

    /// A fake interpreter that prints `label` and its arguments.
    fn interpreter(path: &Path, label: &str) {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, format!("#!/bin/sh\necho {label} \"$@\"\n")).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// Runs `command` through the shims in `cwd`, with the managed
    /// interpreter on the `PATH` behind them.
    fn run(root: &Path, cwd: &Path, command: &str, envs: &[(&str, &str)]) -> String {
        let shims = root.join("shims");
        let path = std::env::join_paths([
            shims,
            root.join("managed"),
            "/usr/bin".into(),
            "/bin".into(),
        ])
        .unwrap();
        let output = Command::new(command)
            .arg("-V")
            .current_dir(cwd)
            .env("PATH", path)
            .env_remove("UV_PROJECT_ENVIRONMENT")
            .envs(envs.iter().copied())
            .output()
            .unwrap();
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    fn fixture() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        futures::executor::block_on(install(&root.path().join("shims"))).unwrap();
        interpreter(&root.path().join("managed/python3"), "managed");
        root
    }

    /// `Command::new` resolves a bare name through the parent's `PATH`, so
    /// the shim is run by its path; the shim itself reads the `PATH` it is given.
    fn shim(root: &Path, name: &str) -> String {
        root.join("shims").join(name).display().to_string()
    }

    #[test]
    fn a_project_environment_in_the_working_directory_is_the_interpreter() {
        let root = fixture();
        let project = root.path().join("project");
        interpreter(&project.join(".venv/bin/python3"), "project");
        let cwd = std::fs::canonicalize(&project).unwrap();
        assert_eq!(
            run(root.path(), &cwd, &shim(root.path(), "python3"), &[]),
            "project -V"
        );
        assert_eq!(
            run(root.path(), &cwd, &shim(root.path(), "python"), &[]),
            "project -V"
        );
    }

    #[test]
    fn the_repository_root_environment_serves_a_subdirectory() {
        let root = fixture();
        let project = root.path().join("project");
        interpreter(&project.join(".venv/bin/python3"), "project");
        std::fs::create_dir_all(project.join(".git")).unwrap();
        std::fs::create_dir_all(project.join("src/deep")).unwrap();
        let cwd = std::fs::canonicalize(project.join("src/deep")).unwrap();
        assert_eq!(
            run(root.path(), &cwd, &shim(root.path(), "python3"), &[]),
            "project -V"
        );
    }

    #[test]
    fn elsewhere_python_is_the_managed_interpreter() {
        let root = fixture();
        let plain = root.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        let cwd = std::fs::canonicalize(&plain).unwrap();
        assert_eq!(
            run(root.path(), &cwd, &shim(root.path(), "python3"), &[]),
            "managed -V"
        );
        assert_eq!(
            run(root.path(), &cwd, &shim(root.path(), "python"), &[]),
            "managed -V"
        );
    }

    #[test]
    fn an_environment_above_the_repository_root_is_not_the_project_s() {
        let root = fixture();
        let outer = root.path().join("outer");
        interpreter(&outer.join(".venv/bin/python3"), "outer");
        std::fs::create_dir_all(outer.join("repo/.git")).unwrap();
        let cwd = std::fs::canonicalize(outer.join("repo")).unwrap();
        assert_eq!(
            run(root.path(), &cwd, &shim(root.path(), "python3"), &[]),
            "managed -V"
        );
    }

    #[test]
    fn uv_project_environment_names_the_environment() {
        let root = fixture();
        let project = root.path().join("project");
        interpreter(&project.join("env/bin/python3"), "named");
        interpreter(&project.join(".venv/bin/python3"), "default");
        let cwd = std::fs::canonicalize(&project).unwrap();
        assert_eq!(
            run(
                root.path(),
                &cwd,
                &shim(root.path(), "python3"),
                &[("UV_PROJECT_ENVIRONMENT", "env")]
            ),
            "named -V"
        );
    }

    #[test]
    fn a_stale_shim_is_rewritten() {
        let root = fixture();
        let shim = root.path().join("shims/python3");
        std::fs::write(&shim, "#!/bin/sh\nexit 1\n").unwrap();
        futures::executor::block_on(install(&root.path().join("shims"))).unwrap();
        assert_eq!(std::fs::read_to_string(&shim).unwrap(), PYTHON_SHIM);
    }
}
