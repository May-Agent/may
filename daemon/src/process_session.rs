//! The process session the daemon leads.
//!
//! Every process the daemon starts — terminal commands, their descendants,
//! MCP servers, browsers — inherits its process session unless it calls
//! `setsid` itself. The daemon therefore leads a session of its own, and a
//! daemon that exits without its shutdown path leaves its session's members
//! behind for the next daemon to end ([`end_exited_session`]): nothing it
//! started outlives an unexpected exit.
//!
//! A process can create a session only when it leads no process group. A
//! daemon started by launchd, or in the foreground of an interactive shell,
//! does lead one; it then runs the daemon as a child in a session of its own
//! and stays its supervisor ([`run_in_own_session`]), forwarding the stop
//! signals and exiting with it.

use std::collections::HashSet;
use std::time::Duration;

use rustix::io::Errno;
use rustix::process::{Pid, Signal};

/// How many times [`end_exited_session`] re-reads the process table to
/// catch a member that forked while the others were killed.
const KILL_ROUNDS: u32 = 100;
/// The pause between two such reads.
const KILL_ROUND_PAUSE: Duration = Duration::from_millis(20);

/// Proof that this process leads its own process session.
#[derive(Debug, Clone, Copy)]
pub struct ProcessSession {
    id: Pid,
}

impl ProcessSession {
    /// Makes this process lead a process session of its own: it already does,
    /// or it creates one. `None` when it leads a process group, which cannot
    /// create a session.
    ///
    /// # Errors
    /// Returns the OS error of `getsid` or `setsid`.
    pub fn lead() -> std::io::Result<Option<Self>> {
        let pid = rustix::process::getpid();
        if rustix::process::getsid(None)? == pid {
            return Ok(Some(Self { id: pid }));
        }
        match rustix::process::setsid() {
            Ok(id) => Ok(Some(Self { id })),
            Err(Errno::PERM) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// The session's id: the pid of this process, its leader.
    #[must_use]
    pub const fn id(self) -> u32 {
        self.id.as_raw_nonzero().get().cast_unsigned()
    }
}

/// Runs this same command as a child that leads a process session of its
/// own, forwarding `SIGINT`, `SIGTERM` and `SIGHUP` to it as `SIGTERM`, and
/// returns once it exited.
///
/// # Errors
/// Fails when the child cannot be started or waited for, or when it exited
/// unsuccessfully.
pub async fn run_in_own_session() -> anyhow::Result<()> {
    use anyhow::Context as _;
    use std::os::unix::process::CommandExt as _;
    use tokio::signal::unix::{SignalKind, signal};

    let mut command = std::process::Command::new(
        std::env::current_exe().context("the daemon's executable has no path")?,
    );
    command.args(std::env::args_os().skip(1));
    // SAFETY: `setsid` is async-signal-safe and touches no memory of the
    // parent; the child is not a process-group leader before it runs.
    unsafe {
        command.pre_exec(|| rustix::process::setsid().map(drop).map_err(Into::into));
    }
    let mut child = async_process::Command::from(command)
        .spawn()
        .context("failed to start the daemon in a process session of its own")?;
    let pid =
        Pid::from_raw(child.id().cast_signed()).context("the daemon's child process has no pid")?;
    tracing::info!(
        pid = child.id(),
        "running the daemon in a process session of its own"
    );

    let listen = |kind| signal(kind).context("failed to listen for stop signals");
    let mut interrupt = listen(SignalKind::interrupt())?;
    let mut terminate = listen(SignalKind::terminate())?;
    let mut hangup = listen(SignalKind::hangup())?;
    let status = loop {
        tokio::select! {
            status = child.status() => break status.context("failed to wait for the daemon")?,
            _ = interrupt.recv() => {}
            _ = terminate.recv() => {}
            _ = hangup.recv() => {}
        }
        match rustix::process::kill_process(pid, Signal::TERM) {
            Ok(()) | Err(Errno::SRCH) => {}
            Err(error) => return Err(error).context("failed to stop the daemon"),
        }
    };
    anyhow::ensure!(status.success(), "the daemon exited with {status}");
    Ok(())
}

/// Ends every process left in the process session `session`, which a daemon
/// that has exited led; returns how many it killed. A live process with the
/// session's id means the session has no members left: a pid is never
/// reused while a session of that id has members.
///
/// Blocking: it signals and re-reads the process table until no member is
/// left.
///
/// # Errors
/// Returns the OS error of reading the process table or of a signal, or an
/// error naming the members that survived every round of `SIGKILL`.
pub fn end_exited_session(session: u32) -> std::io::Result<usize> {
    let session = Pid::from_raw(session.cast_signed()).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{session} is not a process session id"),
        )
    })?;
    match rustix::process::test_kill_process(session) {
        Ok(()) | Err(Errno::PERM) => return Ok(0),
        Err(Errno::SRCH) => {}
        Err(error) => return Err(error.into()),
    }
    let mut killed = HashSet::new();
    for _ in 0..KILL_ROUNDS {
        let members = session_members(session)?;
        if members.is_empty() {
            return Ok(killed.len());
        }
        for member in members {
            match rustix::process::kill_process(member, Signal::KILL) {
                Ok(()) => {
                    killed.insert(member);
                }
                Err(Errno::SRCH) => {}
                Err(error) => return Err(error.into()),
            }
        }
        std::thread::sleep(KILL_ROUND_PAUSE);
    }
    let survivors = session_members(session)?;
    if survivors.is_empty() {
        return Ok(killed.len());
    }
    Err(std::io::Error::other(format!(
        "processes {survivors:?} of the exited daemon's session survived SIGKILL"
    )))
}

/// The live members of process session `session`; exited processes not yet
/// reaped are left out.
#[cfg(target_os = "macos")]
fn session_members(session: Pid) -> std::io::Result<Vec<Pid>> {
    // A zombie has no session to `getsid`, so it drops out here.
    Ok(all_pids()?
        .into_iter()
        .filter_map(Pid::from_raw)
        .filter(|&pid| rustix::process::getsid(Some(pid)) == Ok(session))
        .collect())
}

/// Every pid in the process table.
#[cfg(target_os = "macos")]
fn all_pids() -> std::io::Result<Vec<i32>> {
    let width = std::mem::size_of::<libc::pid_t>();
    loop {
        // SAFETY: a null buffer of size zero asks only for the pid count.
        let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
        let count = usize::try_from(count).map_err(|_| std::io::Error::last_os_error())?;
        // Room for processes started between the two calls.
        let mut pids = vec![0; count + 64];
        let bytes = libc::c_int::try_from(pids.len() * width)
            .map_err(|_| std::io::Error::other("the process table is too large to list"))?;
        // SAFETY: `pids` is live, writable memory of `bytes` bytes.
        let listed = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
        let listed = usize::try_from(listed).map_err(|_| std::io::Error::last_os_error())?;
        // A full buffer may have cut the table short: read it again.
        if listed < pids.len() {
            pids.truncate(listed);
            return Ok(pids);
        }
    }
}

/// The live members of process session `session`; exited processes not yet
/// reaped are left out.
#[cfg(target_os = "linux")]
fn session_members(session: Pid) -> std::io::Result<Vec<Pid>> {
    let session = session.as_raw_nonzero().get();
    let mut members = Vec::new();
    for entry in std::fs::read_dir("/proc")? {
        let entry = entry?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
            .and_then(Pid::from_raw)
        else {
            continue;
        };
        // A process that exits between the listing and this read is gone.
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        // `pid (comm) state ppid pgrp session …`; `comm` may hold spaces and
        // parentheses, so the fields start after the last `)`.
        let mut fields = stat
            .rsplit_once(')')
            .map(|(_, rest)| rest.split_whitespace())
            .ok_or_else(|| std::io::Error::other(format!("malformed /proc stat: {stat}")))?;
        let state = fields.next();
        let member_session = fields.nth(2).and_then(|field| field.parse::<i32>().ok());
        if state != Some("Z") && member_session == Some(session) {
            members.push(pid);
        }
    }
    Ok(members)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
compile_error!("the daemon reads process sessions on macOS and Linux only");

#[cfg(test)]
mod tests {
    use super::*;

    /// A session whose leader exited, with members of its own process
    /// group and of another, is emptied.
    #[test]
    fn an_exited_leaders_session_is_emptied() {
        use std::os::unix::process::CommandExt as _;

        // The leader starts two members (one in a group of its own), prints
        // their pids and exits, so the session outlives its leader.
        let mut command = std::process::Command::new("/bin/sh");
        command
            .args([
                "-c",
                // The members leave stdout, so reading it ends with the leader.
                "sleep 300 >/dev/null 2>&1 & echo $!; (set -m; sleep 300 >/dev/null 2>&1 & echo $!); exit 0",
            ])
            .stdout(std::process::Stdio::piped());
        // SAFETY: `setsid` is async-signal-safe and touches no memory.
        unsafe {
            command.pre_exec(|| rustix::process::setsid().map(drop).map_err(Into::into));
        }
        let child = command.spawn().expect("the leader starts");
        let leader = child.id();
        let output = child.wait_with_output().expect("the leader exits");
        let members: Vec<Pid> = String::from_utf8(output.stdout)
            .expect("pids are text")
            .split_whitespace()
            .map(|pid| Pid::from_raw(pid.parse().expect("a pid")).expect("a nonzero pid"))
            .collect();
        assert_eq!(members.len(), 2, "both members started");
        for &member in &members {
            assert_eq!(
                rustix::process::getsid(Some(member)).map(|sid| sid.as_raw_nonzero().get()),
                Ok(leader.cast_signed()),
                "a member shares the leader's session"
            );
        }

        let killed = end_exited_session(leader).expect("the session is ended");
        assert_eq!(killed, 2);
        for member in members {
            assert_eq!(
                rustix::process::getsid(Some(member)),
                Err(Errno::SRCH),
                "member {member:?} is gone"
            );
        }
    }

    #[test]
    fn a_live_process_with_the_sessions_id_is_left_alone() {
        assert_eq!(end_exited_session(std::process::id()).expect("checked"), 0);
    }
}
