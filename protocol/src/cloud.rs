//! The cloud-machine wire types profiles and sessions carry.

use serde::{Deserialize, Serialize};

/// One machine type of a provider's catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, skyzen::ToSchema)]
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

/// Where a machine is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, skyzen::ToSchema)]
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

/// The GitHub account a profile's codespaces belong to, as `/cloud` set it
/// up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CodespacesAccount {
    /// The account's login, one of the `gh` login's accounts.
    pub login: String,
}

/// A step of a bootstrap, for the caller's loading indicator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootstrapStep {
    /// Checking which build the machine has.
    Checking,
    /// Downloading the build the machine needs.
    Downloading,
    /// Installing it on the machine.
    Installing,
    /// Cloning the session's repository.
    Cloning,
    /// Starting the machine's daemon.
    StartingDaemon,
}

/// Sequential, transient lifecycle facts produced by the browser actor.
///
/// Unlike `BrowserStatusEventPayload`, which is a snapshot reconciled on
/// every topic notification, lifecycle events are point-in-time and must
/// be streamed in order. They ride the existing `/api/events` SSE stream
/// as the `browser_lifecycle` event kind (see `AGENTS.md`).
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum BrowserLifecycleEvent {
    /// Managed browser binary is being downloaded.
    ChromeFetching {
        /// Bytes downloaded so far.
        progress_bytes: u64,
        /// Total download size when the server reports it.
        total_bytes: Option<u64>,
    },
    /// Browser process is launching.
    Starting,
    /// Browser is up and the encoding pipeline is running.
    Ready,
    /// The page is navigating to a new URL.
    Navigating {
        /// URL being navigated to.
        url: String,
    },
    /// A navigation finished and the page is interactive.
    PageLoaded {
        /// URL that finished loading.
        url: String,
        /// Document title after load, when known.
        title: Option<String>,
    },
    /// A WebRTC peer attached to the video stream.
    PeerConnected {
        /// Number of peers attached after this connect.
        count: usize,
    },
    /// A WebRTC peer detached.
    PeerDisconnected {
        /// Number of peers still attached after this disconnect.
        count: usize,
    },
    /// Input takeover ownership moved to another session.
    TakeoverChanged {
        /// Session that now owns input takeover; `None` means the agent owns the page.
        session_id: Option<String>,
    },
    /// The H.264 encoder produced no frame within its watchdog window.
    EncoderStalled,
    /// The browser session shut down.
    Stopped,
    /// An unrecoverable runtime failure occurred.
    Error {
        /// Human-readable failure description.
        message: String,
    },
}
