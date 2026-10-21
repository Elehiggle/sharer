//! One desktop instance per profile, with authenticated loopback show requests.

use std::{
    fs::{self, File, OpenOptions, TryLockError},
    io::{self, Read as _, Write as _},
    net::{SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result};

const LOCK_FILE: &str = "instance.lock";
const ENDPOINT_FILE: &str = "instance.port";
const TOKEN_BYTES: usize = 32;
const ENDPOINT_BYTES: usize = 2 + TOKEN_BYTES;
const IO_TIMEOUT: Duration = Duration::from_millis(350);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(3);
const RETRY_INTERVAL: Duration = Duration::from_millis(25);
const ACK: &[u8; 2] = b"ok";

pub(crate) enum Acquire {
    Primary {
        _guard: InstanceGuard,
        show_rx: Receiver<()>,
    },
    AlreadyRunning,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RepeatLaunch {
    Show,
    KeepHidden,
    RequireProxy,
}

pub(crate) fn acquire(config_dir: &Path, repeat: RepeatLaunch) -> Result<Acquire> {
    fs::create_dir_all(config_dir)
        .with_context(|| format!("failed to create {}", config_dir.display()))?;
    let mut options = OpenOptions::new();

    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;

        options.mode(0o600);
    }

    // Never unlink this file: all contenders must lock the same filesystem object.
    // The kernel releases the lock after crashes, without a stale-file heuristic.
    let lock = options
        .open(config_dir.join(LOCK_FILE))
        .context("failed to open the single-instance lock")?;
    let endpoint_path = config_dir.join(ENDPOINT_FILE);
    let deadline = Instant::now() + STARTUP_TIMEOUT;

    loop {
        match lock.try_lock() {
            Ok(()) => return start_primary(lock, endpoint_path),

            Err(TryLockError::Error(error)) => {
                return Err(error).context("failed to lock the desktop profile");
            }

            Err(TryLockError::WouldBlock) => {}
        }

        anyhow::ensure!(
            repeat != RepeatLaunch::RequireProxy,
            "ShareR is already running; quit it before launching with --require-proxy \
             so the proxy requirement applies to the entire desktop session"
        );

        if repeat == RepeatLaunch::KeepHidden {
            return Ok(Acquire::AlreadyRunning);
        }

        if let Ok(endpoint) = read_endpoint(&endpoint_path)
            && notify_existing(&endpoint).is_ok()
        {
            return Ok(Acquire::AlreadyRunning);
        }

        anyhow::ensure!(
            Instant::now() < deadline,
            "ShareR is already running, but its window could not be requested; try again shortly"
        );
        thread::sleep(RETRY_INTERVAL);
    }
}

fn start_primary(lock: File, endpoint_path: PathBuf) -> Result<Acquire> {
    let listener =
        TcpListener::bind(("127.0.0.1", 0)).context("failed to bind the single-instance socket")?;
    let port = listener.local_addr()?.port();

    listener.set_nonblocking(true)?;
    let mut endpoint = [0_u8; ENDPOINT_BYTES];

    endpoint[..2].copy_from_slice(&port.to_be_bytes());
    getrandom::fill(&mut endpoint[2..])
        .map_err(|error| anyhow::anyhow!("failed to secure the single-instance socket: {error}"))?;
    let (show_tx, show_rx) = mpsc::sync_channel(1);
    let stopping = Arc::new(AtomicBool::new(false));
    let stopping_listener = Arc::clone(&stopping);
    let listener_thread = thread::Builder::new()
        .name("sharer-instance".to_owned())
        .spawn(move || listen(&listener, &endpoint, &show_tx, &stopping_listener))
        .context("failed to start the single-instance listener")?;
    let guard = InstanceGuard {
        _lock: lock,
        endpoint_path,
        stopping,
        listener_thread: Some(listener_thread),
    };
    let directory = guard
        .endpoint_path
        .parent()
        .context("single-instance endpoint has no directory")?;
    let mut file = tempfile::NamedTempFile::new_in(directory)
        .context("failed to create the single-instance endpoint")?;

    // NamedTempFile is owner-only on Unix and inherits the profile ACL on Windows.
    // Publish only a complete endpoint, after the listener is ready.
    file.write_all(&endpoint)?;
    file.persist(&guard.endpoint_path)
        .context("failed to publish the single-instance endpoint")?;

    Ok(Acquire::Primary {
        _guard: guard,
        show_rx,
    })
}

fn listen(
    listener: &TcpListener,
    endpoint: &[u8; ENDPOINT_BYTES],
    show_tx: &SyncSender<()>,
    stopping: &AtomicBool,
) {
    while !stopping.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let _ = serve_request(&mut stream, endpoint, show_tx);
            }

            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::park_timeout(Duration::from_millis(100));
            }

            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}

            Err(_) => break,
        }
    }
}

fn serve_request(
    stream: &mut TcpStream,
    endpoint: &[u8; ENDPOINT_BYTES],
    show_tx: &SyncSender<()>,
) -> io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let mut token = [0_u8; TOKEN_BYTES];

    read_with_deadline(stream, &mut token)?;

    if token != endpoint[2..] {
        return Ok(());
    }

    // Multiple launches need one show operation, not an unbounded queue.
    match show_tx.try_send(()) {
        Ok(()) | Err(TrySendError::Full(())) => stream.write_all(ACK),
        Err(TrySendError::Disconnected(())) => Ok(()),
    }
}

fn read_with_deadline(stream: &mut TcpStream, mut buffer: &mut [u8]) -> io::Result<()> {
    let deadline = Instant::now() + IO_TIMEOUT;

    while !buffer.is_empty() {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "instance request timed out"))?;

        stream.set_read_timeout(Some(remaining))?;

        match stream.read(buffer) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(count) => buffer = &mut buffer[count..],
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }

    Ok(())
}

fn read_endpoint(path: &Path) -> io::Result<[u8; ENDPOINT_BYTES]> {
    let mut endpoint = [0_u8; ENDPOINT_BYTES];

    File::open(path)?.read_exact(&mut endpoint)?;

    Ok(endpoint)
}

fn notify_existing(endpoint: &[u8; ENDPOINT_BYTES]) -> io::Result<()> {
    let port = u16::from_be_bytes([endpoint[0], endpoint[1]]);
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&address, IO_TIMEOUT)?;

    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    stream.write_all(&endpoint[2..])?;
    let mut acknowledgment = [0_u8; ACK.len()];

    read_with_deadline(&mut stream, &mut acknowledgment)?;

    if &acknowledgment != ACK {
        return Err(io::Error::other("invalid single-instance acknowledgment"));
    }

    Ok(())
}

pub(crate) struct InstanceGuard {
    _lock: File,
    endpoint_path: PathBuf,
    stopping: Arc<AtomicBool>,
    listener_thread: Option<JoinHandle<()>>,
}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);

        if let Some(listener) = self.listener_thread.take() {
            listener.thread().unpark();
            let _ = listener.join();
        }

        // Still holding the OS lock; a successor cannot publish its endpoint yet.
        let _ = fs::remove_file(&self.endpoint_path);
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        net::{SocketAddr, TcpStream},
        sync::{Arc, Barrier},
        thread,
        time::Duration,
    };

    use super::{
        Acquire, ENDPOINT_FILE, IO_TIMEOUT, RepeatLaunch, acquire, notify_existing, read_endpoint,
    };

    #[test]
    fn second_instance_notifies_first_and_drop_stops_listener_and_unlocks() {
        let directory = tempfile::tempdir().unwrap();
        let Acquire::Primary {
            _guard: guard,
            show_rx,
        } = acquire(directory.path(), RepeatLaunch::Show).unwrap()
        else {
            panic!("first acquire should own the profile");
        };
        let endpoint = read_endpoint(&directory.path().join(ENDPOINT_FILE)).unwrap();

        assert!(matches!(
            acquire(directory.path(), RepeatLaunch::Show).unwrap(),
            Acquire::AlreadyRunning
        ));
        show_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        drop(guard);
        assert!(notify_existing(&endpoint).is_err());
        assert!(matches!(
            acquire(directory.path(), RepeatLaunch::Show).unwrap(),
            Acquire::Primary { .. }
        ));
    }

    #[test]
    fn background_launch_does_not_show_existing_window() {
        let directory = tempfile::tempdir().unwrap();
        let Acquire::Primary { _guard, show_rx } =
            acquire(directory.path(), RepeatLaunch::KeepHidden).unwrap()
        else {
            panic!("first acquire should own the profile");
        };

        assert!(matches!(
            acquire(directory.path(), RepeatLaunch::KeepHidden).unwrap(),
            Acquire::AlreadyRunning
        ));
        assert!(show_rx.try_recv().is_err());
    }

    #[test]
    fn incomplete_stale_endpoint_does_not_block_startup() {
        let directory = tempfile::tempdir().unwrap();

        fs::write(directory.path().join(ENDPOINT_FILE), []).unwrap();
        assert!(matches!(
            acquire(directory.path(), RepeatLaunch::Show).unwrap(),
            Acquire::Primary { .. }
        ));
    }

    #[test]
    fn unauthenticated_local_request_cannot_show_window() {
        use std::io::{Read as _, Write as _};

        let directory = tempfile::tempdir().unwrap();
        let Acquire::Primary { _guard, show_rx } =
            acquire(directory.path(), RepeatLaunch::Show).unwrap()
        else {
            panic!("first acquire should own the profile");
        };
        let mut endpoint = read_endpoint(&directory.path().join(ENDPOINT_FILE)).unwrap();

        endpoint[2] ^= 1;
        let port = u16::from_be_bytes([endpoint[0], endpoint[1]]);
        let address = SocketAddr::from(([127, 0, 0, 1], port));
        let mut stream = TcpStream::connect_timeout(&address, IO_TIMEOUT).unwrap();

        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        stream.write_all(&endpoint[2..]).unwrap();
        let mut acknowledgment = [0_u8; 2];

        assert!(stream.read_exact(&mut acknowledgment).is_err());
        assert!(show_rx.try_recv().is_err());
    }

    #[test]
    fn simultaneous_launches_elect_exactly_one_primary() {
        let directory = tempfile::tempdir().unwrap();
        let barrier = Arc::new(Barrier::new(8));
        let results = thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    let barrier = Arc::clone(&barrier);
                    let path = directory.path();

                    scope.spawn(move || {
                        barrier.wait();
                        acquire(path, RepeatLaunch::Show).unwrap()
                    })
                })
                .collect();

            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>()
        });

        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, Acquire::Primary { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn proxy_requirement_cannot_be_silently_ignored_by_existing_session() {
        let directory = tempfile::tempdir().unwrap();
        let Acquire::Primary { _guard, show_rx } =
            acquire(directory.path(), RepeatLaunch::Show).unwrap()
        else {
            panic!("first acquire should own the profile");
        };

        assert!(acquire(directory.path(), RepeatLaunch::RequireProxy).is_err());
        assert!(show_rx.try_recv().is_err());
    }
}
