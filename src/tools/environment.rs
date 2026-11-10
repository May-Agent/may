//! The tools prepared in each place a session's commands run: the home
//! environment's, and a server's when it joins.
//!
//! Which programs a session gets is May's choice. The backends take them as
//! launches: [`HostLaunch`], [`ContainerLaunch`] and, for a server,
//! [`PreparedSsh`] with its [`RemoteLaunch`].

use std::path::{Path, PathBuf};
use std::sync::Arc;

use aither_sandbox::{
    AnyContainerExec, ContainerLaunch, ContainerPath, HostLaunch, PreparedSsh, SshServer,
};

use super::distribution::ToolDistribution;
use super::runtime::{HostProgram, PreparedTools, ToolCatalog, ToolRuntimeError, ToolTarget};
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
    ) -> Result<Self, ToolRuntimeError> {
        let target = ToolTarget::native()?;
        ensure_command_line_tools(&LocalToolTransport, &target).await?;
        let artifacts = cache_root.join("artifacts");
        let (bundle, heel) = futures::try_join!(
            distribution.load(&target, &artifacts),
            distribution.prepare_host_program(HostProgram::Heel, &target, cache_root),
        )?;
        let tools = bundle
            .prepare_local(&cache_root.join("runtimes"))
            .await?
            .with_heel(heel);
        Ok(Self::new(
            PreparedHome::Local(tools),
            distribution,
            cache_root,
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
    ) -> Result<Self, ToolRuntimeError> {
        let transport = ContainerToolTransport::new(exec, container_id);
        let info = probe_tool_environment(&transport).await?;
        let tools = distribution
            .image_tools(&info.target)?
            .verify(&transport)
            .await?;
        Ok(Self::new(
            PreparedHome::Container {
                container_id: container_id.into(),
                tools,
            },
            distribution,
            cache_root,
        ))
    }

    fn new(home: PreparedHome, distribution: &ToolDistribution, cache_root: &Path) -> Self {
        Self {
            catalog: home.catalog().clone(),
            home,
            source: Arc::new(ToolSource {
                distribution: distribution.clone(),
                cache_root: cache_root.to_path_buf(),
            }),
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
        self.source
            .distribution
            .load(
                &ToolTarget::native()?,
                &self.source.cache_root.join("artifacts"),
            )
            .await?
            .prepare_local(&self.source.cache_root.join("runtimes"))
            .await
    }

    /// The commands verified in the home environment, to describe to the
    /// model.
    #[must_use]
    pub const fn catalog(&self) -> &ToolCatalog {
        &self.catalog
    }

    /// The server's tools and `heel`: prepared when it joins, so a command
    /// on it never waits for an installation.
    ///
    /// # Errors
    /// Fails when the server cannot be reached, its platform has no pinned
    /// artifacts, or its tools differ from the home environment's.
    pub async fn prepare_ssh(&self, server: &SshServer) -> Result<PreparedSsh, ToolRuntimeError> {
        let transport = SshToolTransport::new(server.clone());
        let info = probe_tool_environment(&transport).await?;
        ensure_command_line_tools(&transport, &info.target).await?;
        let bundle = self
            .source
            .distribution
            .load(&info.target, &self.source.cache_root.join("artifacts"))
            .await?;
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
        Self::new(PreparedHome::Local(tools), distribution, cache_root)
    }

    /// The environment of container tools a test prepared by hand.
    #[cfg(test)]
    pub(crate) fn with_test_container_tools(
        tools: PreparedTools<ContainerPath>,
        container_id: String,
        distribution: &ToolDistribution,
        cache_root: &Path,
    ) -> Self {
        Self::new(
            PreparedHome::Container {
                container_id,
                tools,
            },
            distribution,
            cache_root,
        )
    }
}

/// The server's `heel`: the one it has, or the distribution's pinned `heel`
/// for the server's own target, installed there.
///
/// # Errors
/// Fails when the server has no `heel` and the distribution pins none for its
/// target.
async fn remote_heel(
    transport: &SshToolTransport,
    target: &ToolTarget,
    source: &ToolSource,
) -> Result<Option<String>, ToolRuntimeError> {
    let found = successful_output(
        transport
            .run(include_str!("../../templates/ssh_heel_probe.sh"), None)
            .await?,
    )?;
    if let Some(path) = found.lines().next() {
        return Ok(Some(path.to_string()));
    }
    let local = source
        .distribution
        .prepare_host_program(HostProgram::Heel, target, &source.cache_root)
        .await?;
    let binary = async_fs::read(local.path()).await?;
    let installed = successful_output(
        transport
            .run(
                include_str!("../../templates/ssh_heel_install.sh"),
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
