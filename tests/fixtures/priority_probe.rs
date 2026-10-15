//! Kernel-scheduling probe for macOS: prints `parentNice parentIO childNice
//! childIO`. The parent spawns an exec'd copy of itself; both read their own
//! CPU nice level and disk I/O policy straight from the kernel, so a run shows
//! the ambient priority and whether a descendant inherits it. Linux uses
//! `priority_probe.sh`, since its sandbox executes only system binaries.

#[cfg(not(target_os = "macos"))]
compile_error!("the compiled priority probe is macOS-only; Linux uses priority_probe.sh");

use std::ffi::{OsStr, c_int};
use std::io;
use std::process::{Command, exit};

const PRIO_PROCESS: c_int = 0;
const IOPOL_TYPE_DISK: c_int = 0;
const IOPOL_SCOPE_PROCESS: c_int = 0;

unsafe extern "C" {
    fn getpriority(which: c_int, who: c_int) -> c_int;
    fn getiopolicy_np(iotype: c_int, scope: c_int) -> c_int;
    fn __error() -> *mut c_int;
}

fn errno() -> &'static mut c_int {
    unsafe { &mut *__error() }
}

fn nice() -> io::Result<i32> {
    // getpriority can legitimately return -1, so errno carries the failure.
    unsafe {
        *errno() = 0;
        let value = getpriority(PRIO_PROCESS, 0);
        if value == -1 && *errno() != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(value)
    }
}

fn io_priority() -> io::Result<i32> {
    let value = unsafe { getiopolicy_np(IOPOL_TYPE_DISK, IOPOL_SCOPE_PROCESS) };
    if value == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(value)
}

fn observe() -> io::Result<(i32, i32)> {
    Ok((nice()?, io_priority()?))
}

fn run() -> io::Result<()> {
    let (nice, io) = observe()?;
    let is_child = std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == OsStr::new("--child"));
    if is_child {
        println!("{nice} {io}");
        return Ok(());
    }

    let exe = std::env::current_exe()?;
    let output = Command::new(exe).arg("--child").output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "child probe exited with {}",
            output.status
        )));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let values: Vec<i32> = text
        .split_whitespace()
        .map(str::parse)
        .collect::<Result<_, _>>()
        .map_err(io::Error::other)?;
    let [child_nice, child_io] = values
        .try_into()
        .map_err(|_| io::Error::other("child probe did not print two integers"))?;
    println!("{nice} {io} {child_nice} {child_io}");
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("priority_probe: {error}");
        exit(1);
    }
}
