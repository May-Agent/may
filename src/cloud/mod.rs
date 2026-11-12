//! Cloud machines: a provider's catalog and the lifecycle of the machines it
//! creates.
//!
//! A [`CloudProvider`] creates a machine from a source — what the machine
//! starts with — and starts, stops, resizes and deletes it. A machine is a
//! place commands run, reached like an SSH server, or the home environment of
//! a session that runs in the cloud; both uses drive the provider through
//! this one trait. GitHub Codespaces ([`codespaces`]) is the first provider.
//!
//! Which provider a profile uses, and as which account, is the profile's
//! [`ProfileCloud`], set with `/cloud`.

pub mod codespaces;

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::workspace::GitHubRepository;

pub use codespaces::{Codespaces, CodespacesAccount};

/// A cloud provider: where machines come from and how their lifecycle is
/// driven.
///
/// Every operation answers with what the provider reports at once; a machine
/// in [`MachineState::Provisioning`] settles later, and [`machine`] reads
/// where it is.
///
/// [`machine`]: CloudProvider::machine
pub trait CloudProvider: Send + Sync {
    /// What a machine is created from, resolved by [`source`].
    ///
    /// [`source`]: CloudProvider::source
    type Source: Send + Sync;

    /// The source a machine serving `project` is created from: the project
    /// repository, or the provider's own when the session has none.
    ///
    /// # Errors
    /// Fails when the provider cannot be reached, refuses, or the source it
    /// would use is unsafe to create machines from.
    fn source(
        &self,
        project: Option<&GitHubRepository>,
    ) -> impl Future<Output = Result<Self::Source, CloudError>> + Send;

    /// The machine types a machine from `source` may be created on.
    ///
    /// # Errors
    /// Fails when the provider cannot be reached or refuses.
    fn catalog(
        &self,
        source: &Self::Source,
    ) -> impl Future<Output = Result<Vec<MachineType>, CloudError>> + Send;

    /// Creates a machine from `source`.
    ///
    /// # Errors
    /// Fails when the provider cannot be reached or refuses.
    fn create(
        &self,
        source: &Self::Source,
        request: &NewMachine,
    ) -> impl Future<Output = Result<CloudMachine, CloudError>> + Send;

    /// The machine `id`, or `None` when the provider no longer has it.
    ///
    /// # Errors
    /// Fails when the provider cannot be reached or refuses.
    fn machine(
        &self,
        id: &MachineId,
    ) -> impl Future<Output = Result<Option<CloudMachine>, CloudError>> + Send;

    /// Starts a suspended machine on the disk it kept.
    ///
    /// # Errors
    /// [`CloudError::Gone`] when the machine no longer exists; otherwise
    /// fails when the provider cannot be reached or refuses.
    fn start(
        &self,
        id: &MachineId,
    ) -> impl Future<Output = Result<CloudMachine, CloudError>> + Send;

    /// Suspends a machine, keeping its disk.
    ///
    /// # Errors
    /// [`CloudError::Gone`] when the machine no longer exists; otherwise
    /// fails when the provider cannot be reached or refuses.
    fn stop(&self, id: &MachineId)
    -> impl Future<Output = Result<CloudMachine, CloudError>> + Send;

    /// Moves a machine to `machine_type`, keeping its disk. A running
    /// machine runs on its current type until it is next started.
    ///
    /// # Errors
    /// [`CloudError::Gone`] when the machine no longer exists; otherwise
    /// fails when the provider cannot be reached or refuses.
    fn resize(
        &self,
        id: &MachineId,
        machine_type: &str,
    ) -> impl Future<Output = Result<CloudMachine, CloudError>> + Send;

    /// Deletes a machine and its disk. A machine the provider no longer has
    /// is already deleted.
    ///
    /// # Errors
    /// Fails when the provider cannot be reached or refuses.
    fn delete(&self, id: &MachineId) -> impl Future<Output = Result<(), CloudError>> + Send;
}

/// The provider's identifier of a machine.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MachineId(String);

impl MachineId {
    /// The machine the provider names `id`.
    #[must_use]
    pub const fn new(id: String) -> Self {
        Self(id)
    }

    /// The identifier as the provider spells it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for MachineId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One machine type of a provider's catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MachineType {
    /// The provider's name of the type, which a create or resize names.
    pub name: String,
    /// What the provider calls it in a picker.
    pub display_name: String,
    /// CPU cores.
    pub cpus: u32,
    /// Memory, in bytes.
    pub memory_bytes: u64,
    /// Disk, in bytes.
    pub storage_bytes: u64,
}

/// What a new machine is created with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewMachine {
    /// The catalog's [`MachineType::name`] to create it on.
    pub machine_type: String,
    /// The name the provider shows the user for it.
    pub display_name: String,
}

/// A machine as the provider reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudMachine {
    /// The provider's identifier.
    pub id: MachineId,
    /// Where it is in its lifecycle.
    pub state: MachineState,
    /// The type it runs on, when the provider has assigned one.
    pub machine_type: Option<MachineType>,
}

/// Where a machine is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MachineState {
    /// Being created, started, stopped or changed; it settles on its own.
    Provisioning,
    /// Running and reachable.
    Running,
    /// Not running; its disk is kept and a start resumes it.
    Suspended,
    /// Deleted, or dead where it is: nothing starts it again.
    Gone,
}

/// The cloud provider accounts a profile uses, one field per provider.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProfileCloud {
    /// The GitHub account the profile's codespaces belong to, when set up.
    pub codespaces: Option<CodespacesAccount>,
}

/// Why a cloud provider operation failed.
#[derive(Debug, thiserror::Error)]
pub enum CloudError {
    /// The provider's credential could not be read, or the request did not
    /// reach the provider.
    #[error(transparent)]
    Transport(#[from] crate::issue::IssueError),
    /// The provider answered with a refusal.
    #[error("{provider} answered HTTP {status}: {message}")]
    Refused {
        /// The provider's name.
        provider: &'static str,
        /// The HTTP status.
        status: u16,
        /// The provider's own message.
        message: String,
    },
    /// The provider's answer is not the documented shape.
    #[error("{provider} answered in an unexpected shape: {message}")]
    Decode {
        /// The provider's name.
        provider: &'static str,
        /// What failed to decode.
        message: String,
    },
    /// The machine no longer exists.
    #[error("cloud machine {0} no longer exists")]
    Gone(MachineId),
    /// The GitHub account's `gh` token was not granted a scope the provider
    /// needs.
    #[error(
        "the GitHub account {login} was not granted the `{scope}` scope; run `gh auth refresh --hostname github.com --scopes {scope}` while {login} is gh's active account"
    )]
    MissingGitHubScope {
        /// The account.
        login: String,
        /// The scope it lacks.
        scope: &'static str,
    },
    /// The source a machine would be created from is public, so other
    /// accounts could open the machine.
    #[error(
        "{0} is public: a machine created from it could be opened by other accounts, so May will not create one there"
    )]
    PublicSource(GitHubRepository),
}
