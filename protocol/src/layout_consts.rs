//! Layout constants shared across the workspace.

/// The kind of a script that runs on an SSH server; the request's reason is
/// the server's target.
pub const SSH_KIND: &str = "ssh";

/// Where a container's host workspace directory is bind-mounted, and the
/// working directory of its commands. Every host-to-container workspace path
/// derives from this one definition.
pub const CONTAINER_WORKSPACE: &str = "/workspace";
