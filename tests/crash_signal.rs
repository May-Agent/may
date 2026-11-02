//! The fatal-signal dump writes the log ring to `crash-*.log` when a signal,
//! not a panic, ends the process. Each case re-executes this binary with an
//! env-var child mode so the fatal signal takes down a disposable process.
#![cfg(unix)]

use std::io::Write as _;
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};

use tracing_subscriber::fmt::MakeWriter as _;

const CHILD_ENV: &str = "MAY_CRASH_SIGNAL_TEST_CHILD";
const BUGS_ENV: &str = "MAY_CRASH_SIGNAL_TEST_BUGS";
const RING_LINE: &[u8] = b"a recognizable ring line 7b3e\n";

/// The child half: write a recognizable line to the ring, install the dump,
/// then end the process by the requested signal.
fn child(mode: &str) {
    let bugs_dir = PathBuf::from(std::env::var_os(BUGS_ENV).expect("bugs dir env"));
    let (writer_maker, ring) = may_agent::log_ring::LogRingHandle::new(64 * 1024);
    may_agent::crash_signal::install(ring, bugs_dir).expect("the crash dump installs");

    // Write the line through the ring's own writer so the dump has content.
    let mut writer = writer_maker.make_writer();
    writer.write_all(RING_LINE).expect("ring write");

    match mode {
        "overflow" => {
            std::hint::black_box(overflow());
        }
        "abort" => unsafe { libc::abort() },
        _ => panic!("unknown child mode {mode}"),
    }
}

/// Recurse without a base case until the stack overflows; `black_box` keeps
/// the frames from collapsing into a tail call. The call goes through a
/// function pointer so the recursion is real, not optimized or linted away.
#[inline(never)]
fn overflow() -> usize {
    let f: fn() -> usize = std::hint::black_box(overflow);
    f() + std::hint::black_box(1usize)
}

/// Spawn the child and return its output.
fn spawn(mode: &str, bugs_dir: &Path) -> std::process::Output {
    use std::os::unix::process::CommandExt as _;
    let mut command = std::process::Command::new(std::env::current_exe().expect("test binary"));
    command
        .env(CHILD_ENV, mode)
        .env(BUGS_ENV, bugs_dir)
        .arg("--exact")
        .arg(format!("crash_{mode}"));
    // The test harness would otherwise run the child as another test; this
    // flag makes it a no-op harness run with the env-var side effect first.
    unsafe {
        command.pre_exec(|| Ok(()));
    }
    command.output().expect("spawn child")
}

/// The one `crash-*.log` the child must have written.
fn crash_logs(bugs_dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(bugs_dir)
        .expect("bugs dir")
        .map(|entry| entry.expect("dir entry").path())
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("crash-"))
        })
        .collect()
}

#[test]
fn crash_overflow() {
    if let Ok(mode) = std::env::var(CHILD_ENV) {
        child(&mode);
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let bugs_dir = dir.path().join(".may").join("bugs");
    let output = spawn("overflow", &bugs_dir);

    assert!(
        output.status.signal().is_some(),
        "the child died of a signal: {:?}",
        output.status
    );
    eprintln!("child stderr: {}", String::from_utf8_lossy(&output.stderr));

    let logs = crash_logs(&bugs_dir);
    assert_eq!(
        logs.len(),
        1,
        "the overflow ends in one abort after the dump: {logs:?}"
    );
    let contents = std::fs::read_to_string(&logs[0]).expect("crash log");
    // macOS reports a guard-page fault as SIGBUS, Linux as SIGSEGV; either is
    // the overflow's real signal and the dump records it.
    assert!(
        contents.contains("signal SIGSEGV") || contents.contains("signal SIGBUS"),
        "{contents}"
    );
    assert!(contents.contains("a recognizable ring line"), "{contents}");
}

#[test]
fn crash_abort() {
    if let Ok(mode) = std::env::var(CHILD_ENV) {
        child(&mode);
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let bugs_dir = dir.path().join(".may").join("bugs");
    let output = spawn("abort", &bugs_dir);

    assert_eq!(
        output.status.signal(),
        Some(libc::SIGABRT),
        "abort ends the child: {:?}",
        output.status
    );

    let logs = crash_logs(&bugs_dir);
    assert_eq!(logs.len(), 1, "one dump per crash: {logs:?}");
    let contents = std::fs::read_to_string(&logs[0]).expect("crash log");
    assert!(contents.contains("signal SIGABRT"), "{contents}");
    assert!(contents.contains("a recognizable ring line"), "{contents}");
}
