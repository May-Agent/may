//! The tools prepared in each place a session's commands run: the home
//! environment's, and a server's when it joins.
//!
//! Which programs a session gets is May's choice. The backends take them as
//! launches: [`HostLaunch`], [`ContainerLaunch`] and, for a server,
//! [`PreparedSsh`] with its [`RemoteLaunch`].

use std::path::{Path, PathBuf};
use std::sync::Arc;

use askama::Template as _;
use sha2::Digest as _;

use aither_sandbox::{
    AnyContainerExec, ContainerLaunch, ContainerPath, HostLaunch, PrepareSsh, PreparedSsh,
    SshServer,
};

use super::distribution::ToolDistribution;
use super::runtime::{
    HostProgram, ManagedTool, PreparedTools, ToolBundle, ToolCatalog, ToolRuntimeError, ToolTarget,
};
use super::transport::{
    ContainerToolTransport, LocalToolTransport, SshToolTransport, ToolTransport,
    ensure_command_line_tools, probe_tool_environment, successful_output,
};

/// The home backend's tools, typed by where that backend sees them.
#[derive(Debug, Clone)]
enum PreparedHome {
    Local(PreparedTools<PathBuf>),
    Container {
        container_id: String,
        tools: PreparedTools<ContainerPath>,
    },
}

impl PreparedHome {
    const fn catalog(&self) -> &ToolCatalog {
        match self {
            Self::Local(tools) => tools.catalog(),
            Self::Container { tools, .. } => tools.catalog(),
        }
    }
}

/// What preparing an SSH server draws on: the distribution the home
/// environment was prepared from and its cache.
#[derive(Debug)]
struct ToolSource {
    distribution: ToolDistribution,
    cache_root: PathBuf,
}

/// The tools of a session's home environment, prepared before an agent can
/// use it, and the means to prepare a server's when it joins.
#[derive(Debug, Clone)]
pub struct ToolEnvironments {
    home: PreparedHome,
    catalog: ToolCatalog,
    source: Arc<ToolSource>,
    /// `[experimental] hashline`: `apply_patch` stays out of every backend.
    hashline: bool,
}

/// The bundle a backend gets: `apply_patch` stripped under hashline mode.
fn strip_for_hashline(bundle: ToolBundle, hashline: bool) -> ToolBundle {
    if hashline {
        bundle.without(ManagedTool::ApplyPatch)
    } else {
        bundle
    }
}

impl ToolEnvironments {
    /// Prepares the host as the home environment.
    ///
    /// # Errors
    /// Fails on unsupported platforms, missing artifacts and integrity
    /// failures.
    pub async fn prepare_host(
        distribution: &ToolDistribution,
        cache_root: &Path,
        hashline: bool,
    ) -> Result<Self, ToolRuntimeError> {
        let target = ToolTarget::native()?;
        ensure_command_line_tools(&LocalToolTransport, &target).await?;
        let artifacts = cache_root.join("artifacts");
        let (bundle, heel) = futures::try_join!(
            distribution.load(&target, &artifacts),
            distribution.prepare_host_program(HostProgram::Heel, &target, cache_root),
        )?;
        let tools = strip_for_hashline(bundle, hashline)
            .prepare_local(&cache_root.join("runtimes"))
            .await?
            .with_heel(heel);
        Ok(Self::new(
            PreparedHome::Local(tools),
            distribution,
            cache_root,
            hashline,
        ))
    }

    /// Prepares the container `container_id`, reached through `exec`, as the
    /// home environment.
    ///
    /// # Errors
    /// Fails on unsupported platforms, missing artifacts and integrity
    /// failures.
    pub async fn prepare_container(
        exec: AnyContainerExec,
        container_id: &str,
        distribution: &ToolDistribution,
        cache_root: &Path,
        hashline: bool,
    ) -> Result<Self, ToolRuntimeError> {
        let transport = ContainerToolTransport::new(exec, container_id);
        let info = probe_tool_environment(&transport).await?;
        let image_tools = distribution.image_tools(&info.target)?;
        let image_tools = if hashline {
            image_tools.without(ManagedTool::ApplyPatch)
        } else {
            image_tools
        };
        let tools = image_tools.verify(&transport).await?;
        Ok(Self::new(
            PreparedHome::Container {
                container_id: container_id.into(),
                tools,
            },
            distribution,
            cache_root,
            hashline,
        ))
    }

    fn new(
        home: PreparedHome,
        distribution: &ToolDistribution,
        cache_root: &Path,
        hashline: bool,
    ) -> Self {
        Self {
            catalog: home.catalog().clone(),
            home,
            source: Arc::new(ToolSource {
                distribution: distribution.clone(),
                cache_root: cache_root.to_path_buf(),
            }),
            hashline,
        }
    }

    /// The prepared host shell, for `Backend::Host`.
    ///
    /// # Errors
    /// Fails when the home environment is a container or the shell path is
    /// not text.
    pub fn host_launch(&self) -> Result<HostLaunch, ToolRuntimeError> {
        Ok(HostLaunch::new(self.local_tools()?.shell()?))
    }

    /// The prepared programs of the home container, for `Backend::Container`.
    ///
    /// # Errors
    /// Fails when the home environment is the host.
    pub fn container_launch(&self) -> Result<ContainerLaunch, ToolRuntimeError> {
        match &self.home {
            PreparedHome::Container { tools, .. } => Ok(tools.container_launch()),
            PreparedHome::Local(_) => Err(ToolRuntimeError::NotInitialized(
                "container environment".into(),
            )),
        }
    }

    /// The id of the container the home environment was prepared in.
    #[must_use]
    pub fn container_id(&self) -> Option<&str> {
        match &self.home {
            PreparedHome::Container { container_id, .. } => Some(container_id),
            PreparedHome::Local(_) => None,
        }
    }

    /// The prepared tools of a local home environment, as the host sees them.
    ///
    /// # Errors
    /// Fails when the home environment is a container, whose tools the host
    /// cannot run; use [`Self::host_tools`] for tools the host runs.
    pub fn local_tools(&self) -> Result<&PreparedTools<PathBuf>, ToolRuntimeError> {
        match &self.home {
            PreparedHome::Local(tools) => Ok(tools),
            PreparedHome::Container { .. } => {
                Err(ToolRuntimeError::NotInitialized("local environment".into()))
            }
        }
    }

    /// Baseline tools runnable on the host, for programs the host itself runs.
    ///
    /// A local home environment's prepared tools are returned as they are.
    /// When the home environment is a container, its tools target another
    /// platform, so the same distribution prepares the host target into the
    /// same cache with the same pinning and verification.
    ///
    /// # Errors
    /// Fails when the distribution has no verified artifacts for the host
    /// platform.
    pub async fn host_tools(&self) -> Result<PreparedTools<PathBuf>, ToolRuntimeError> {
        if let PreparedHome::Local(tools) = &self.home {
            return Ok(tools.clone());
        }
        strip_for_hashline(
            self.source
                .distribution
                .load(
                    &ToolTarget::native()?,
                    &self.source.cache_root.join("artifacts"),
                )
                .await?,
            self.hashline,
        )
        .prepare_local(&self.source.cache_root.join("runtimes"))
        .await
    }

    /// The commands verified in the home environment, to describe to the
    /// model.
    #[must_use]
    pub const fn catalog(&self) -> &ToolCatalog {
        &self.catalog
    }

    /// Prepares `server`: its `heel` and the home environment's tools.
    ///
    /// # Errors
    /// Fails when the server cannot be reached, its platform has no pinned
    /// artifacts, or its tools differ from the home environment's.
    async fn prepare_server(&self, server: &SshServer) -> Result<PreparedSsh, ToolRuntimeError> {
        let transport = SshToolTransport::new(server.clone());
        let info = probe_tool_environment(&transport).await?;
        ensure_command_line_tools(&transport, &info.target).await?;
        let bundle = strip_for_hashline(
            self.source
                .distribution
                .load(&info.target, &self.source.cache_root.join("artifacts"))
                .await?,
            self.hashline,
        );
        let (tools, heel) = futures::try_join!(
            bundle.prepare_remote(&transport, &info.cache_root),
            remote_heel(&transport, &info.target, &self.source),
        )?;
        if tools.catalog() != &self.catalog {
            return Err(ToolRuntimeError::CatalogMismatch);
        }
        Ok(PreparedSsh::new(heel, tools.remote_launch()?))
    }

    /// The environment of tools a test prepared by hand.
    #[cfg(test)]
    pub(crate) fn with_test_tools(
        tools: PreparedTools<PathBuf>,
        distribution: &ToolDistribution,
        cache_root: &Path,
    ) -> Self {
        Self::new(PreparedHome::Local(tools), distribution, cache_root, false)
    }
}

/// A server is prepared when the first command names it, once, and again
/// after a failure: adding a server never waits on an installation.
impl PrepareSsh for ToolEnvironments {
    type Error = ToolRuntimeError;

    fn prepare(
        &self,
        server: &SshServer,
    ) -> impl Future<Output = Result<PreparedSsh, Self::Error>> + Send {
        self.prepare_server(server)
    }
}

/// `ssh_heel_probe.sh`, rendered with the pinned executable's SHA-256.
#[derive(askama::Template)]
#[template(path = "ssh_heel_probe.sh", escape = "none")]
struct HeelProbe<'a> {
    expected: &'a str,
}

/// `ssh_heel_install.sh`, rendered with the pinned executable's SHA-256.
#[derive(askama::Template)]
#[template(path = "ssh_heel_install.sh", escape = "none")]
struct HeelInstall<'a> {
    expected: &'a str,
}

/// The server's `heel`: a heel it already has when that heel is the
/// distribution's pinned binary — by version and SHA-256, never by name —
/// or the pinned `heel` for the server's own target, installed
/// digest-addressed.
///
/// # Errors
/// Fails when the server has no pinned `heel` and the distribution pins
/// none for its target.
async fn remote_heel(
    transport: &SshToolTransport,
    target: &ToolTarget,
    source: &ToolSource,
) -> Result<Option<String>, ToolRuntimeError> {
    let local = source
        .distribution
        .prepare_host_program(HostProgram::Heel, target, &source.cache_root)
        .await?;
    let binary = async_fs::read(local.path()).await?;
    let expected = hex::encode(sha2::Sha256::digest(&binary));
    let found = successful_output(
        transport
            .run(
                &HeelProbe {
                    expected: &expected,
                }
                .render()
                .expect("the heel probe template renders typed input"),
                None,
            )
            .await?,
    )?;
    if let Some(path) = found.lines().next() {
        return Ok(Some(path.to_string()));
    }
    let installed = successful_output(
        transport
            .run(
                &HeelInstall {
                    expected: &expected,
                }
                .render()
                .expect("the heel install template renders typed input"),
                Some(binary),
            )
            .await?,
    )?;
    let path = installed
        .lines()
        .next()
        .ok_or(ToolRuntimeError::InvalidProbe)?;
    Ok(Some(path.to_string()))
}

#[cfg(all(test, unix))]
mod tests {
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::Path;
    use std::process::{Command, Stdio};

    use sha2::Digest as _;

    use super::{HeelInstall, HeelProbe};
    use askama::Template as _;

    /// A `heel` look-alike: a script that answers `--version`.
    fn fake_heel(dir: &Path) -> std::path::PathBuf {
        let heel = dir.join("heel");
        std::fs::write(
            &heel,
            "#!/bin/sh\n[ \"$1\" = --version ] && echo 'heel 9.9.9'\n",
        )
        .unwrap();
        std::fs::set_permissions(&heel, std::fs::Permissions::from_mode(0o755)).unwrap();
        heel
    }

    fn run(script: &str, home: &Path, stdin: &[u8]) -> String {
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(script)
            .env("HOME", home)
            .env("PATH", "/usr/bin:/bin")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(stdin).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success() || script.contains("probe"),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// A `heel` on the server is trusted only when its digest is the pinned
    /// one; any other binary named `heel` is rejected.
    #[test]
    fn the_probe_accepts_only_the_pinned_heel() {
        let home = tempfile::tempdir().unwrap();
        let bin = home.path().join(".local/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let planted = fake_heel(&bin);
        // The server has a `heel` whose digest is not the pin's.
        let rejected = run(
            &HeelProbe {
                expected: &"0".repeat(64),
            }
            .render()
            .unwrap(),
            home.path(),
            &[],
        );
        assert!(rejected.is_empty(), "{rejected}");
        // When it is, the probe names it.
        let digest = hex::encode(sha2::Sha256::digest(std::fs::read(&planted).unwrap()));
        let accepted = run(
            &HeelProbe { expected: &digest }.render().unwrap(),
            home.path(),
            &[],
        );
        assert_eq!(accepted, planted.to_str().unwrap(), "{accepted}");
    }

    /// The upload is staged through `mktemp` and lands, digest-addressed,
    /// only after its version and digest check out.
    #[test]
    fn the_install_stages_and_addresses_the_pinned_heel() {
        let home = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let binary = std::fs::read(fake_heel(work.path())).unwrap();
        let digest = hex::encode(sha2::Sha256::digest(&binary));
        let installed = run(
            &HeelInstall { expected: &digest }.render().unwrap(),
            home.path(),
            &binary,
        );
        let expected_path = home
            .path()
            .join(".local/bin")
            .join(format!("heel-{digest}"));
        assert_eq!(installed, expected_path.to_str().unwrap(), "{installed}");
        assert_eq!(std::fs::read(&expected_path).unwrap(), binary);
        // No staging file is left behind.
        assert_eq!(
            std::fs::read_dir(expected_path.parent().unwrap())
                .unwrap()
                .count(),
            1
        );
        // A corrupted upload never lands.
        let mut bad = binary;
        bad[0] = b'!';
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(HeelInstall { expected: &digest }.render().unwrap())
            .env("HOME", home.path())
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(&bad).unwrap();
        assert!(!child.wait().unwrap().success());
    }
}
