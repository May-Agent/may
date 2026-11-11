//! The `python3`, `python` and `uv` commands of a terminal: the interpreter
//! of the project environment that holds the working directory, else that of
//! the session's own environment, `~/.python-env` (`~` is the session
//! directory), which `uv venv` creates from the managed `python3` the first
//! time a command needs it. `uv pip` and `uv run` get that environment as
//! `VIRTUAL_ENV` only where no project environment is found. The environment
//! lives in the session directory and goes with it. The container image
//! installs the same template (see `container::stage`), in front of its own
//! `python3` and `uv`.
//!
//! The managed `python3` is a verified link inside the prepared tools
//! directory, so the commands live in the tools' shim directory
//! ([`PreparedTools::shim_dir`](crate::tools::PreparedTools::shim_dir)),
//! which leads the session's `PATH` and is read-only to the sandbox like the
//! tools themselves. They resolve at run time, from the directory the command
//! runs in.

use std::path::Path;

/// The shim every command is.
const PYTHON_SHIM: &str = include_str!("../../templates/python_shim");

/// The commands the shim is installed as.
const COMMANDS: [&str; 3] = ["python3", "python", "uv"];

/// Installs the shims into `directory`, rewriting one whose content differs
/// from the template. Every session installs into the same directory, so a
/// shim is replaced by a rename and a command already running it keeps its
/// file.
#[cfg(unix)]
pub(super) async fn install(directory: &Path) -> std::io::Result<()> {
    async_fs::create_dir_all(directory).await?;
    for command in COMMANDS {
        let shim = directory.join(command);
        match async_fs::read(&shim).await {
            Ok(current) if current == PYTHON_SHIM.as_bytes() => continue,
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let directory = directory.to_path_buf();
        blocking::unblock(move || {
            use std::io::Write as _;
            use std::os::unix::fs::PermissionsExt as _;

            let mut file = tempfile::NamedTempFile::new_in(&directory)?;
            file.write_all(PYTHON_SHIM.as_bytes())?;
            file.as_file()
                .set_permissions(std::fs::Permissions::from_mode(0o755))?;
            file.persist(&shim).map_err(|error| error.error)?;
            Ok::<_, std::io::Error>(())
        })
        .await?;
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
        run_args(root, cwd, command, &["-V"], envs)
    }

    /// The session directory of the fixture: the shims' `HOME`.
    fn home(root: &Path) -> std::path::PathBuf {
        root.join("home")
    }

    fn run_args(
        root: &Path,
        cwd: &Path,
        command: &str,
        args: &[&str],
        envs: &[(&str, &str)],
    ) -> String {
        let shims = root.join("shims");
        let path = std::env::join_paths([
            shims,
            root.join("managed"),
            "/usr/bin".into(),
            "/bin".into(),
        ])
        .unwrap();
        let output = Command::new(command)
            .args(args)
            .current_dir(cwd)
            .env("PATH", path)
            .env("HOME", home(root))
            .env_remove("UV_PROJECT_ENVIRONMENT")
            .env_remove("VIRTUAL_ENV")
            .envs(envs.iter().copied())
            .output()
            .unwrap();
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    fn fixture() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        futures::executor::block_on(install(&root.path().join("shims"))).unwrap();
        interpreter(&root.path().join("managed/python3"), "managed");
        std::fs::create_dir_all(home(root.path())).unwrap();
        fake_uv(&root.path().join("managed/uv"));
        root
    }

    /// A fake `uv`: `venv ... <directory>` builds an environment whose
    /// interpreters print `session`, anything else prints the `VIRTUAL_ENV` it
    /// runs with and its arguments. `venv` is slow enough for racing
    /// commands to overlap.
    fn fake_uv(path: &Path) {
        use std::os::unix::fs::PermissionsExt as _;
        let script = r#"#!/bin/sh
if [ "$1" = venv ]; then
    for directory; do :; done
    sleep 0.2
    mkdir -p "$directory/bin"
    for python in python python3; do
        printf '#!/bin/sh\necho session "$@"\n' > "$directory/bin/$python"
        chmod 755 "$directory/bin/$python"
    done
    echo created >> "$HOME/venv-runs"
    exit 0
fi
echo "uv venv=${VIRTUAL_ENV:-unset} $*"
"#;
        std::fs::write(path, script).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn plain_directory(root: &Path) -> std::path::PathBuf {
        let plain = root.join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        std::fs::canonicalize(&plain).unwrap()
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
    fn elsewhere_python_is_the_session_environment() {
        let root = fixture();
        let cwd = plain_directory(root.path());
        assert!(!home(root.path()).join(".python-env").exists());
        assert_eq!(
            run(root.path(), &cwd, &shim(root.path(), "python3"), &[]),
            "session -V"
        );
        assert_eq!(
            run(root.path(), &cwd, &shim(root.path(), "python"), &[]),
            "session -V"
        );
        // Created once, inside the session directory.
        let runs = std::fs::read_to_string(home(root.path()).join("venv-runs")).unwrap();
        assert_eq!(runs.lines().count(), 1);
    }

    #[test]
    fn outside_a_project_uv_pip_installs_into_the_session_environment() {
        let root = fixture();
        let cwd = plain_directory(root.path());
        let uv = shim(root.path(), "uv");
        let session = home(root.path()).join(".python-env");
        assert_eq!(
            run_args(root.path(), &cwd, &uv, &["pip", "install", "x"], &[]),
            format!("uv venv={} pip install x", session.display())
        );
        // The same environment is the one `python3` runs.
        assert_eq!(
            run(root.path(), &cwd, &shim(root.path(), "python3"), &[]),
            "session -V"
        );
        let runs = std::fs::read_to_string(home(root.path()).join("venv-runs")).unwrap();
        assert_eq!(runs.lines().count(), 1);
    }

    #[test]
    fn inside_a_project_uv_leaves_virtual_env_unset() {
        let root = fixture();
        let project = root.path().join("project");
        interpreter(&project.join(".venv/bin/python3"), "project");
        std::fs::create_dir_all(project.join(".git")).unwrap();
        std::fs::create_dir_all(project.join("src")).unwrap();
        let cwd = std::fs::canonicalize(project.join("src")).unwrap();
        assert_eq!(
            run_args(
                root.path(),
                &cwd,
                &shim(root.path(), "uv"),
                &["pip", "install", "x"],
                &[]
            ),
            "uv venv=unset pip install x"
        );
        assert!(!home(root.path()).join(".python-env").exists());
    }

    #[test]
    fn uv_commands_that_need_no_environment_create_none() {
        let root = fixture();
        let cwd = plain_directory(root.path());
        let uv = shim(root.path(), "uv");
        assert_eq!(
            run_args(root.path(), &cwd, &uv, &["--version"], &[]),
            "uv venv=unset --version"
        );
        assert_eq!(
            run_args(root.path(), &cwd, &uv, &["python", "list"], &[]),
            "uv venv=unset python list"
        );
        assert!(!home(root.path()).join(".python-env").exists());
    }

    #[test]
    fn an_activated_environment_is_left_alone() {
        let root = fixture();
        let cwd = plain_directory(root.path());
        assert_eq!(
            run_args(
                root.path(),
                &cwd,
                &shim(root.path(), "uv"),
                &["pip", "list"],
                &[("VIRTUAL_ENV", "/elsewhere")]
            ),
            "uv venv=/elsewhere pip list"
        );
        assert!(!home(root.path()).join(".python-env").exists());
    }

    #[test]
    fn racing_commands_create_the_session_environment_once() {
        let root = fixture();
        let cwd = plain_directory(root.path());
        let outputs = std::thread::scope(|scope| {
            let handles = (0..8)
                .map(|index| {
                    let (root, cwd) = (root.path(), &cwd);
                    scope.spawn(move || {
                        let name = if index % 2 == 0 { "python3" } else { "uv" };
                        let args: &[&str] = if name == "uv" {
                            &["pip", "list"]
                        } else {
                            &["-V"]
                        };
                        run_args(root, cwd, &shim(root, name), args, &[])
                    })
                })
                .collect::<Vec<_>>();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>()
        });
        let session = home(root.path()).join(".python-env");
        for (index, output) in outputs.iter().enumerate() {
            let expected = if index % 2 == 0 {
                "session -V".to_string()
            } else {
                format!("uv venv={} pip list", session.display())
            };
            assert_eq!(output, &expected);
        }
        // One built environment, the one published; the losers' are gone.
        let built = std::fs::read_dir(home(root.path()))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with(".python-env."))
            })
            .collect::<Vec<_>>();
        assert_eq!(built, [std::fs::read_link(&session).unwrap()]);
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
            "session -V"
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
