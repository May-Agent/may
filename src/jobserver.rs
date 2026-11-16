//! The machine-wide jobserver token pool.
//!
//! The daemon owns one pool as a `fifo:` jobserver at `~/.may/jobserver/pool`.
//! Every terminal tool opens its own connection to that fifo and brokers the
//! tokens to its commands (see [`aither_sandbox::job_broker_channel`]), so the
//! jobs of all sessions, subagents and scheduled commands together never
//! exceed the pool size.
//!
//! The pool resizes in place. Growing writes the new tokens into the fifo.
//! Shrinking takes tokens out of it as they become available: tokens that
//! running commands hold come back to the pool when those commands exit and
//! are discarded there, so a shrink never interrupts work and the total
//! settles on the new size.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::num::NonZeroUsize;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use aither_sandbox::{JobBroker, JobPool, JobserverError, job_broker_channel};
use async_io::Async;
use executor_core::{DefaultExecutor, Executor};
use futures_lite::future;

const TOKEN: u8 = b'+';

/// Directory under the May home that holds the pool and the per-command fifos.
const DIR: &str = "jobserver";

/// Where the pool and the command fifos live under the May home.
fn layout(may_dir: &Path) -> (PathBuf, PathBuf) {
    let dir = may_dir.join(DIR);
    (dir.join("pool"), dir.join("commands"))
}

/// Connects to the pool of the daemon running under `may_dir`.
///
/// For a client process whose commands draw from it. The daemon must be
/// running: its start creates the pool, and a stale pool of a stopped daemon
/// is replaced.
///
/// # Errors
/// Returns an error when the pool fifo cannot be opened.
pub fn connect_running(may_dir: &Path) -> Result<JobConnection, JobserverError> {
    let (pool_path, fifo_dir) = layout(may_dir);
    Ok(JobConnection {
        pool: JobPool::fifo(&pool_path)?,
        fifo_dir,
    })
}

/// The pool size when none is configured: the host's available parallelism.
#[must_use]
pub fn default_pool_size() -> NonZeroUsize {
    std::thread::available_parallelism().unwrap_or(NonZeroUsize::MIN)
}

/// A terminal's way into the pool: its own connection to the pool fifo and
/// the directory its command fifos live in.
#[derive(Debug)]
pub struct JobConnection {
    /// The connection handed to the terminal tool.
    pub pool: JobPool,
    /// Broker-owned directory for the terminal's per-command fifos.
    pub fifo_dir: PathBuf,
}

/// Handle on the daemon's pool; clones share it.
#[derive(Debug, Clone)]
pub struct JobServer {
    pool_path: PathBuf,
    fifo_dir: PathBuf,
    resize: async_channel::Sender<usize>,
}

impl JobServer {
    /// Creates the pool fifo under `may_dir` seeded with `size` tokens,
    /// replacing one a previous daemon left behind, and starts the task that
    /// applies resizes.
    ///
    /// # Errors
    /// Returns an error when the fifo cannot be created or opened.
    pub async fn start(may_dir: &Path, size: NonZeroUsize) -> Result<Self, JobserverError> {
        let (pool_path, fifo_dir) = layout(may_dir);
        let dir = may_dir.join(DIR);
        let (read, write) = {
            let dir = dir.clone();
            let pool_path = pool_path.clone();
            let fifo_dir = fifo_dir.clone();
            blocking::unblock(move || -> Result<_, JobserverError> {
                std::fs::create_dir_all(&dir)?;
                // Fifos of a previous daemon hold tokens nobody accounts for.
                match std::fs::remove_dir_all(&fifo_dir) {
                    Err(error) if error.kind() != io::ErrorKind::NotFound => {
                        return Err(error.into());
                    }
                    _ => {}
                }
                match std::fs::remove_file(&pool_path) {
                    Err(error) if error.kind() != io::ErrorKind::NotFound => {
                        return Err(error.into());
                    }
                    _ => {}
                }
                // A fifo drops its buffered tokens once every end is closed,
                // so the seeding connection lives until ours are open.
                let seed = JobPool::create_fifo(&pool_path, size.get())?;
                let read = OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NONBLOCK)
                    .open(&pool_path)?;
                let write = OpenOptions::new()
                    .write(true)
                    .custom_flags(libc::O_NONBLOCK)
                    .open(&pool_path)?;
                drop(seed);
                Ok((read, write))
            })
            .await?
        };
        let (resize, targets) = async_channel::unbounded();
        let mut tokens = Tokens {
            read: Async::new(read)?,
            write: Async::new(write)?,
            size: size.get(),
            debt: 0,
        };
        DefaultExecutor
            .spawn(async move { tokens.serve(targets).await })
            .detach();
        Ok(Self {
            pool_path,
            fifo_dir,
            resize,
        })
    }

    /// Opens a connection for one terminal tool.
    ///
    /// # Errors
    /// Returns an error when the pool fifo cannot be opened.
    pub fn connect(&self) -> Result<JobConnection, JobserverError> {
        Ok(JobConnection {
            pool: JobPool::fifo(&self.pool_path)?,
            fifo_dir: self.fifo_dir.clone(),
        })
    }

    /// Opens a broker for commands the caller starts itself, with its
    /// service running on the global executor: the connection of one
    /// terminal, for processes that do not run through a terminal tool.
    ///
    /// # Errors
    /// Returns an error when the pool fifo cannot be opened or the broker
    /// cannot be set up.
    pub fn broker(&self) -> Result<JobBroker, JobserverError> {
        let connection = self.connect()?;
        let (broker, service) = job_broker_channel(connection.pool, connection.fifo_dir)?;
        DefaultExecutor
            .spawn(async move { service.serve().await })
            .detach();
        Ok(broker)
    }

    /// Tokens sitting in the pool fifo right now.
    #[cfg(test)]
    pub(crate) fn tokens_in_pool(&self) -> usize {
        let pool = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&self.pool_path)
            .unwrap();
        usize::try_from(rustix::io::ioctl_fionread(&pool).unwrap()).unwrap()
    }

    /// Sets the pool size. Growth is immediate; shrinking completes as
    /// commands return tokens.
    ///
    /// # Panics
    /// Panics if the resize task stopped, which only a panic in it causes.
    pub fn resize(&self, size: NonZeroUsize) {
        // The task lives as long as any handle does.
        self.resize
            .try_send(size.get())
            .expect("the jobserver resize task outlives its handles");
    }
}

/// The resize task's view of the pool fifo.
struct Tokens {
    read: Async<File>,
    write: Async<File>,
    /// Tokens the pool should hold in total.
    size: usize,
    /// Tokens still to be taken out of the pool to reach `size`.
    debt: usize,
}

enum Wake {
    Target(Result<usize, async_channel::RecvError>),
    Absorbed(io::Result<()>),
}

impl Tokens {
    async fn serve(&mut self, targets: async_channel::Receiver<usize>) {
        loop {
            let target = async { Wake::Target(targets.recv().await) };
            let wake = if self.debt == 0 {
                target.await
            } else {
                future::or(target, async { Wake::Absorbed(self.absorb().await) }).await
            };
            match wake {
                Wake::Target(Ok(target)) => self.retarget(target).await,
                Wake::Target(Err(_)) => return,
                Wake::Absorbed(result) => {
                    result.expect("jobserver pool read failed");
                    self.debt -= 1;
                }
            }
        }
    }

    async fn retarget(&mut self, target: usize) {
        if target >= self.size {
            let grow = target - self.size;
            let cancelled = grow.min(self.debt);
            self.debt -= cancelled;
            for _ in 0..grow - cancelled {
                self.write
                    .write_with(|mut file| file.write_all(&[TOKEN]))
                    .await
                    .expect("jobserver pool write failed");
            }
        } else {
            self.debt += self.size - target;
        }
        self.size = target;
    }

    /// Takes one token out of the pool, waiting for one to be available.
    async fn absorb(&self) -> io::Result<()> {
        self.read
            .read_with(|mut file| file.read_exact(&mut [0u8; 1]))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Bytes buffered in a fifo opened for reading.
    fn buffered(file: &File) -> usize {
        usize::try_from(rustix::io::ioctl_fionread(file).unwrap()).unwrap()
    }

    fn nz(n: usize) -> NonZeroUsize {
        NonZeroUsize::new(n).unwrap()
    }

    async fn settle(what: &str, mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !condition() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            async_io::Timer::after(Duration::from_millis(5)).await;
        }
    }

    /// A command fifo opened read-write the way make does, plus the tokens
    /// the command has taken.
    struct Command {
        file: File,
        held: usize,
    }

    impl Command {
        async fn open(broker: &aither_sandbox::JobBroker) -> (aither_sandbox::CommandFifo, Self) {
            let lease = broker.open_fifo().await.unwrap();
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(lease.path())
                .unwrap();
            (lease, Self { file, held: 0 })
        }

        /// Takes every token currently available, like a job-hungry build.
        fn take_all(&mut self) {
            while (&self.file).read(&mut [0u8; 1]).is_ok_and(|n| n == 1) {
                self.held += 1;
            }
        }
    }

    /// One terminal tool's broker over a connection to the shared pool.
    fn terminal(server: &JobServer) -> aither_sandbox::JobBroker {
        server.broker().unwrap()
    }

    fn observer(server: &JobServer) -> File {
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&server.pool_path)
            .unwrap()
    }

    async fn server(size: usize) -> (tempfile::TempDir, JobServer) {
        crate::test_support::install_global_executor();
        let dir = tempfile::tempdir().unwrap();
        let server = JobServer::start(dir.path(), nz(size)).await.unwrap();
        (dir, server)
    }

    #[tokio::test]
    async fn terminals_on_one_pool_never_hold_more_than_its_size() {
        const SIZE: usize = 3;
        let (_dir, server) = server(SIZE).await;
        let (first, second) = (terminal(&server), terminal(&server));
        let (lease_a, mut a) = Command::open(&first).await;
        let (lease_b, mut b) = Command::open(&second).await;

        settle("both terminals to drain the pool", || {
            a.take_all();
            b.take_all();
            a.held + b.held == SIZE
        })
        .await;
        for _ in 0..20 {
            async_io::Timer::after(Duration::from_millis(5)).await;
            a.take_all();
            b.take_all();
            assert!(a.held + b.held <= SIZE, "over-subscribed");
        }
        assert_eq!(buffered(&observer(&server)), 0);
        drop((lease_a, lease_b, a, b));
    }

    #[tokio::test]
    async fn growing_adds_tokens_to_every_terminal() {
        let (_dir, server) = server(2).await;
        let (first, second) = (terminal(&server), terminal(&server));
        let (_lease_a, mut a) = Command::open(&first).await;
        let (_lease_b, mut b) = Command::open(&second).await;
        settle("the pool to drain", || {
            a.take_all();
            b.take_all();
            a.held + b.held == 2
        })
        .await;

        server.resize(nz(5));
        settle("the new tokens to be handed out", || {
            a.take_all();
            b.take_all();
            a.held + b.held == 5
        })
        .await;
    }

    #[tokio::test]
    async fn shrinking_retires_tokens_as_commands_return_them() {
        let (_dir, server) = server(4).await;
        let broker = terminal(&server);
        let (lease, mut command) = Command::open(&broker).await;
        settle("the command to take every token", || {
            command.take_all();
            command.held == 4
        })
        .await;

        server.resize(nz(1));
        // The command's tokens are still out: nothing is interrupted.
        async_io::Timer::after(Duration::from_millis(50)).await;
        command.take_all();
        assert_eq!(command.held, 4);

        // Once the command is gone its tokens return, and all but one are
        // retired.
        drop((lease, command));
        let pool = observer(&server);
        settle("the pool to settle on one token", || buffered(&pool) == 1).await;
        async_io::Timer::after(Duration::from_millis(100)).await;
        assert_eq!(buffered(&pool), 1);
    }

    #[tokio::test]
    async fn growing_cancels_an_unpaid_shrink() {
        let (_dir, server) = server(4).await;
        let broker = terminal(&server);
        let (lease, mut command) = Command::open(&broker).await;
        settle("the command to take every token", || {
            command.take_all();
            command.held == 4
        })
        .await;

        server.resize(nz(1));
        server.resize(nz(3));
        drop((lease, command));
        let pool = observer(&server);
        settle("the pool to settle on three tokens", || {
            buffered(&pool) == 3
        })
        .await;
        async_io::Timer::after(Duration::from_millis(100)).await;
        assert_eq!(buffered(&pool), 3);
    }

    #[tokio::test]
    async fn start_replaces_a_pool_left_by_a_previous_daemon() {
        let (dir, _first) = server(2).await;
        let second = JobServer::start(dir.path(), nz(5)).await.unwrap();
        assert_eq!(buffered(&observer(&second)), 5);
    }
}
