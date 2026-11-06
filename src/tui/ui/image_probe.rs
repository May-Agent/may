//! Terminal graphics probe with a bounded read.
//!
//! ratatui-image's own stdio query runs on a detached thread that keeps
//! blocking in `stdin.read` after its timeout and restores the termios it
//! saved when the first byte finally arrives, eating that byte and leaving
//! the terminal cooked. This probe runs on the caller's thread: it writes the
//! capability query, reads replies one byte at a time under a poll deadline
//! and restores the terminal mode before it returns, so nothing outlives it.

use std::io::{self, Write};
use std::os::fd::AsFd;
use std::time::Duration;

use ratatui_image::picker::ProtocolType;
use ratatui_image::picker::cap_parser::{Parser, QueryStdioOptions, Response};
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::termios::{OptionalActions, tcgetattr, tcsetattr};

/// What the terminal answered to the capability query.
#[derive(Debug, Default, PartialEq)]
pub(super) struct Reply {
    /// The graphics protocol the terminal acknowledged.
    pub protocol: Option<ProtocolType>,
    /// The cell size in pixels, `(width, height)`.
    pub cell_size: Option<(u16, u16)>,
}

/// Terminals that acknowledge Kitty or Sixel without implementing them
/// usably; iTerm2 detection from the environment serves them better.
pub(super) fn ignores_kitty_and_sixel() -> bool {
    ["WEZTERM_EXECUTABLE", "KONSOLE_VERSION"]
        .iter()
        .any(|name| std::env::var(name).is_ok_and(|value| !value.is_empty()))
}

/// Asks the terminal on `input`/`output` for its graphics capabilities.
///
/// `None` when no reply ends the query within `timeout` of the last byte.
/// `input` is put in raw mode for the read and restored before returning on
/// every path.
///
/// # Errors
/// Fails when the terminal mode cannot be read or set, or on an I/O error.
pub(super) fn query<I: AsFd, O: Write>(
    input: &I,
    output: &mut O,
    is_tmux: bool,
    timeout: Duration,
    ignore_kitty_and_sixel: bool,
) -> io::Result<Option<Reply>> {
    let original = tcgetattr(input)?;
    let mut raw = original.clone();
    raw.make_raw();
    tcsetattr(input, OptionalActions::Now, &raw)?;
    let replies = read_replies(input, output, is_tmux, timeout);
    tcsetattr(input, OptionalActions::Now, &original)?;
    Ok(replies?.map(|replies| interpret(&replies, ignore_kitty_and_sixel)))
}

fn read_replies<I: AsFd, O: Write>(
    input: &I,
    output: &mut O,
    is_tmux: bool,
    timeout: Duration,
) -> io::Result<Option<Vec<Response>>> {
    let mut options = QueryStdioOptions::default();
    options.timeout = timeout;
    output.write_all(Parser::query(is_tmux, options).as_bytes())?;
    output.flush()?;

    let wait = Timespec {
        tv_sec: i64::try_from(timeout.as_secs()).expect("a probe timeout fits i64"),
        tv_nsec: i64::from(timeout.subsec_nanos()),
    };
    let mut parser = Parser::new();
    let mut replies = Vec::new();
    loop {
        let mut fds = [PollFd::new(input, PollFlags::IN)];
        if poll(&mut fds, Some(&wait))? == 0 {
            return Ok(None);
        }
        let mut byte = [0u8; 1];
        if rustix::io::read(input, &mut byte)? == 0 {
            return Ok(None);
        }
        let mut parsed = parser.push(char::from(byte[0]));
        // The status report answers the last query: everything else is in.
        if matches!(parsed[..], [Response::Status]) {
            return Ok(Some(replies));
        }
        replies.append(&mut parsed);
    }
}

fn interpret(replies: &[Response], ignore_kitty_and_sixel: bool) -> Reply {
    let mut reply = Reply::default();
    for response in replies {
        match response {
            Response::Kitty if !ignore_kitty_and_sixel => {
                reply.protocol = Some(ProtocolType::Kitty);
            }
            Response::Sixel if !ignore_kitty_and_sixel && reply.protocol.is_none() => {
                reply.protocol = Some(ProtocolType::Sixel);
            }
            Response::CellSize(Some(size)) => reply.cell_size = Some(*size),
            _ => {}
        }
    }
    reply
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustix::termios::{InputModes, LocalModes};
    use std::fs::File;
    use std::io::Read;
    use std::os::fd::{FromRawFd, OwnedFd};

    /// A pty pair: the terminal side plays the emulator, the other side is
    /// the tty the probe runs on.
    fn pty() -> (File, OwnedFd) {
        let (mut master, mut slave) = (0, 0);
        // SAFETY: both out-pointers are valid; the null name, termios and
        // winsize arguments are accepted by openpty.
        let rc = unsafe {
            libc::openpty(
                &raw mut master,
                &raw mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(rc, 0, "openpty");
        // SAFETY: openpty returned two fresh descriptors this test now owns.
        unsafe { (File::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) }
    }

    /// The line-discipline flags a raw probe changes. The kernel's transient
    /// `PENDIN` retype flag is not one of them.
    fn mode(fd: &OwnedFd) -> String {
        let termios = tcgetattr(fd).expect("tcgetattr");
        let canonical = termios.local_modes.contains(LocalModes::ICANON);
        let echo = termios.local_modes.contains(LocalModes::ECHO);
        let cr = termios.input_modes.contains(InputModes::ICRNL);
        format!("icanon={canonical} echo={echo} icrnl={cr}")
    }

    /// A terminal that never answers: the probe gives up at its deadline,
    /// leaves the mode as it found it, and a key typed afterwards is still
    /// there to read — nothing swallows it.
    #[test]
    fn a_silent_terminal_leaves_the_mode_and_the_next_key_alone() {
        let (mut master, slave) = pty();
        let before = mode(&slave);
        let mut sink = Vec::new();

        let started = std::time::Instant::now();
        let reply =
            query(&slave, &mut sink, false, Duration::from_millis(100), false).expect("probe");
        assert_eq!(reply, None);
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(mode(&slave), before, "the mode is restored");
        assert!(!sink.is_empty(), "the query was written");

        master.write_all(b"x").expect("type");
        let mut slave = File::from(slave);
        // Cooked mode holds a lone byte until a newline; switch to raw to
        // read what is queued.
        let fd = slave.try_clone().expect("clone");
        let mut raw = tcgetattr(&fd).expect("tcgetattr");
        raw.make_raw();
        tcsetattr(&fd, OptionalActions::Now, &raw).expect("tcsetattr");
        let mut byte = [0u8; 1];
        slave.read_exact(&mut byte).expect("read");
        assert_eq!(&byte, b"x");
    }

    /// A terminal that answers the cell-size query and the status report
    /// yields its cell size.
    #[test]
    fn an_answering_terminal_reports_its_cell_size() {
        let (mut master, slave) = pty();
        master
            .write_all(b"\x1b[6;20;10t\x1b[0n")
            .expect("terminal reply");
        let mut sink = Vec::new();
        let reply = query(&slave, &mut sink, false, Duration::from_secs(2), false)
            .expect("probe")
            .expect("a reply");
        assert_eq!(reply.cell_size, Some((10, 20)));
        assert_eq!(reply.protocol, None);
    }
}
