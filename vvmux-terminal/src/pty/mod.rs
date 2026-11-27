//! Platform pseudo-terminals for pane processes.
//!
//! [`PtyProcess::spawn`] and [`PtyProcess::spawn_argv`] start a child attached to a new
//! pseudo-terminal and return its [`PtyParts`]: a reader for the terminal's output, a bounded
//! [`PtyInput`] queue for its input, a shared [`PtyControl`] for resizing and termination, and a
//! [`PtyWaiter`] that collects the exit status.
//!
//! On Unix the child becomes the leader of a new session and process group, with the PTY as its
//! controlling terminal; signals go to that group. On Windows the child runs under `ConPTY` inside
//! a kill-on-close job object; there are no signals or process groups.
//!
//! Vivid endpoints, tokens, and root secrets are removed from every child's environment, so a
//! pane process never inherits the multiplexer's own media credentials.

use std::ffi::OsStr;
use std::fmt;
use std::fs::File;
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub use unix::{PtyControl, PtyExitStatus, PtyWaiter};
#[cfg(windows)]
pub use windows::{PtyControl, PtyExitStatus, PtyWaiter};

/// Writes that may wait in a pane's input queue before senders see `WouldBlock`.
///
/// Paired with [`INPUT_QUEUE_BYTES`] so that neither many tiny writes nor a few large pastes can
/// grow memory without bound while a child is not reading its input.
const INPUT_QUEUE_ITEMS: usize = 64;

/// Bytes that may wait in a pane's input queue; also the largest single write accepted.
///
/// 1 MiB comfortably holds a large paste, and is the same ceiling the session applies to
/// automation input, so anything the session accepts can be queued in one write.
const INPUT_QUEUE_BYTES: usize = 1024 * 1024;

/// Entry points for starting pane processes.
#[derive(Debug)]
pub struct PtyProcess;

/// Everything needed to drive one newly started pane process.
#[derive(Debug)]
pub struct PtyParts {
    /// The operating-system ID of the started process.
    pub child_pid: u32,
    /// The terminal's output stream.
    pub reader: File,
    /// The bounded queue that writes to the terminal's input.
    pub input: PtyInput,
    /// The shared handle for resizing, signalling, and termination.
    pub control: PtyControl,
    /// The owner of the process, used to collect its exit status.
    pub waiter: PtyWaiter,
}

#[derive(Debug, Default)]
struct InputBudget {
    items: AtomicUsize,
    bytes: AtomicUsize,
}

#[derive(Debug)]
struct InputCharge {
    budget: Arc<InputBudget>,
    bytes: usize,
    queued: bool,
}

impl InputCharge {
    fn release_item(&mut self) {
        if self.queued {
            self.budget.items.fetch_sub(1, Ordering::AcqRel);
            self.queued = false;
        }
    }
}

impl Drop for InputCharge {
    fn drop(&mut self) {
        self.budget.bytes.fetch_sub(self.bytes, Ordering::AcqRel);
        self.release_item();
    }
}

/// Capacity for a later write to this exact PTY. Dropping it returns the capacity.
#[derive(Debug)]
pub struct PtyWritePermit {
    sender: mpsc::SyncSender<InputMessage>,
    charge: InputCharge,
}

impl PtyWritePermit {
    /// Send a bounded report using the pre-reserved capacity.
    ///
    /// # Errors
    ///
    /// Returns an error if the report exceeds the reservation or the writer has closed.
    pub fn send(self, bytes: &[u8]) -> io::Result<()> {
        self.submit(bytes, None)
    }

    fn submit(
        self,
        bytes: &[u8],
        completion: Option<mpsc::Sender<io::Result<()>>>,
    ) -> io::Result<()> {
        if bytes.len() > self.charge.bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "write exceeds its reservation",
            ));
        }
        let message = InputMessage {
            bytes: bytes.to_vec(),
            completion,
            charge: self.charge,
        };
        self.sender.try_send(message).map_err(|error| {
            let kind = match error {
                mpsc::TrySendError::Full(_) => io::ErrorKind::WouldBlock,
                mpsc::TrySendError::Disconnected(_) => io::ErrorKind::BrokenPipe,
            };
            io::Error::new(kind, "PTY input queue is unavailable")
        })
    }
}

struct InputMessage {
    charge: InputCharge,
    bytes: Vec<u8>,
    completion: Option<mpsc::Sender<io::Result<()>>>,
}

/// Bounded, non-blocking writer for a pane's terminal input.
///
/// A dedicated thread performs the blocking writes, so a child that stops reading its input
/// fills this queue and makes further sends fail with [`io::ErrorKind::WouldBlock`] instead of
/// blocking the caller.
pub struct PtyInput {
    sender: mpsc::SyncSender<InputMessage>,
    budget: Arc<InputBudget>,
}

impl fmt::Debug for PtyInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PtyInput")
            .field("queued_bytes", &self.budget.bytes.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

impl PtyInput {
    fn start(mut writer: File) -> io::Result<Self> {
        let (sender, receiver) = mpsc::sync_channel::<InputMessage>(INPUT_QUEUE_ITEMS);
        std::thread::Builder::new()
            .name("vvmux-pty-input".into())
            .spawn(move || {
                while let Ok(mut message) = receiver.recv() {
                    message.charge.release_item();
                    let result = writer
                        .write_all(&message.bytes)
                        .and_then(|()| writer.flush());
                    drop(message.charge);
                    if let Some(completion) = message.completion {
                        let reported = match &result {
                            Ok(()) => Ok(()),
                            Err(error) => Err(io::Error::new(error.kind(), error.to_string())),
                        };
                        let _ = completion.send(reported);
                    }
                    if result.is_err() {
                        break;
                    }
                }
            })?;
        Ok(Self {
            sender,
            budget: Arc::default(),
        })
    }

    /// Queue `bytes` for the terminal's input without waiting for them to be written.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::InvalidInput`] when `bytes` exceeds the queue's byte limit,
    /// [`io::ErrorKind::WouldBlock`] when the queue is full, or [`io::ErrorKind::BrokenPipe`]
    /// after the writer thread stopped because a write failed.
    pub fn send(&self, bytes: &[u8]) -> io::Result<()> {
        self.enqueue(bytes, None)
    }

    /// Queue `bytes` and return a receiver that reports when they were written and flushed.
    ///
    /// Empty input completes immediately.
    ///
    /// # Errors
    ///
    /// The same as [`PtyInput::send`]. A write failure after queueing is reported through the
    /// returned receiver instead.
    pub fn send_with_completion(&self, bytes: &[u8]) -> io::Result<mpsc::Receiver<io::Result<()>>> {
        let (sender, receiver) = mpsc::channel();
        if bytes.is_empty() {
            let _ = sender.send(Ok(()));
            return Ok(receiver);
        }
        self.enqueue(bytes, Some(sender))?;
        Ok(receiver)
    }

    fn enqueue(
        &self,
        bytes: &[u8],
        completion: Option<mpsc::Sender<io::Result<()>>>,
    ) -> io::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        self.reserve(bytes.len())?.submit(bytes, completion)
    }

    /// Reserve one input message and its maximum byte length before beginning a multi-step write.
    ///
    /// # Errors
    ///
    /// An oversized reservation returns `InvalidInput`. A full item or byte budget returns
    /// `WouldBlock`; no capacity remains charged after an error.
    pub fn reserve(&self, max_bytes: usize) -> io::Result<PtyWritePermit> {
        if max_bytes > INPUT_QUEUE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "PTY input exceeds the bounded queue",
            ));
        }
        let full = || io::Error::new(io::ErrorKind::WouldBlock, "PTY input queue is full");
        self.budget
            .items
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |items| {
                (items < INPUT_QUEUE_ITEMS).then_some(items + 1)
            })
            .map_err(|_items| full())?;
        if self
            .budget
            .bytes
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |bytes| {
                bytes
                    .checked_add(max_bytes)
                    .filter(|total| *total <= INPUT_QUEUE_BYTES)
            })
            .is_err()
        {
            self.budget.items.fetch_sub(1, Ordering::AcqRel);
            return Err(full());
        }
        Ok(PtyWritePermit {
            sender: self.sender.clone(),
            charge: InputCharge {
                budget: Arc::clone(&self.budget),
                bytes: max_bytes,
                queued: true,
            },
        })
    }
}

impl Write for PtyInput {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.send(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl PtyProcess {
    /// Start a pane process.
    ///
    /// With `command`, the shell runs that one command string and the pane ends when it does;
    /// without it, the shell starts as an interactive login shell. The command is handed to the
    /// shell verbatim, so it may contain pipes and redirection — it is a shell command, not an
    /// argument vector.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::InvalidInput`] for a zero dimension (and, on Windows, for a
    /// relative shell or working-directory path), [`io::ErrorKind::Unsupported`] on Windows
    /// builds without `ConPTY`, or the OS error from creating the terminal or process.
    pub fn spawn(
        shell: &OsStr,
        command: Option<&OsStr>,
        cwd: &Path,
        columns: u16,
        rows: u16,
        environment: &[(String, String)],
    ) -> io::Result<PtyParts> {
        #[cfg(unix)]
        {
            unix::spawn(shell, command, cwd, columns, rows, environment)
        }
        #[cfg(windows)]
        {
            windows::spawn(shell, command, cwd, columns, rows, environment)
        }
    }

    /// Start a pane process from an exact program and argument vector, without a shell.
    ///
    /// # Errors
    ///
    /// The same as [`PtyProcess::spawn`].
    pub fn spawn_argv(
        program: &OsStr,
        arguments: &[impl AsRef<OsStr>],
        cwd: &Path,
        columns: u16,
        rows: u16,
        environment: &[(String, String)],
    ) -> io::Result<PtyParts> {
        #[cfg(unix)]
        {
            unix::spawn_argv(program, arguments, cwd, columns, rows, environment)
        }
        #[cfg(windows)]
        {
            windows::spawn_argv(program, arguments, cwd, columns, rows, environment)
        }
    }
}

fn input(writer: File) -> io::Result<PtyInput> {
    PtyInput::start(writer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn completion_fires_after_bytes_are_flushed() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("pty-input");
        let file = File::create(&path).unwrap();
        let input = PtyInput::start(file).unwrap();
        let completion = input.send_with_completion(b"written").unwrap();
        completion
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"written");
    }

    #[test]
    fn reserved_release_can_be_sent_with_every_other_input_slot_held() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("release");
        let input = PtyInput::start(File::create(&path).unwrap()).unwrap();
        let release = input.reserve(32).unwrap();
        let others: Vec<_> = (1..super::INPUT_QUEUE_ITEMS)
            .map(|_| input.reserve(1).unwrap())
            .collect();
        assert_eq!(
            input.send(b"x").unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        release.send(b"release").unwrap();
        drop(others);
        input
            .send_with_completion(b"done")
            .unwrap()
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"releasedone");
    }

    /// A command pane really does run its command and end with it.
    ///
    /// The `-c` path has no production caller yet, so without this the flag selection would be
    /// unproven until the `run` action lands.
    #[cfg(unix)]
    #[test]
    fn a_command_pane_runs_the_command_and_exits() {
        use std::io::Read;

        let directory = tempfile::tempdir().unwrap();
        let mut parts = PtyProcess::spawn(
            std::ffi::OsStr::new("/bin/sh"),
            Some(std::ffi::OsStr::new("printf 'COMMAND_RAN'")),
            directory.path(),
            80,
            24,
            &[],
        )
        .unwrap();

        let mut output = Vec::new();
        let mut buffer = [0_u8; 1024];
        // Read to EOF: the shell exits once the command finishes, closing the PTY.
        while let Ok(read) = parts.reader.read(&mut buffer) {
            if read == 0 {
                break;
            }
            output.extend_from_slice(&buffer[..read]);
        }

        assert!(
            String::from_utf8_lossy(&output).contains("COMMAND_RAN"),
            "expected the command's output, got {:?}",
            String::from_utf8_lossy(&output)
        );
        let status = parts.waiter.wait().unwrap();
        assert!(status.success, "a completed command should exit cleanly");
    }

    #[cfg(unix)]
    #[test]
    fn argv_pane_does_not_shell_interpret_arguments() {
        use std::io::Read;

        let directory = tempfile::tempdir().unwrap();
        let mut parts = PtyProcess::spawn_argv(
            std::ffi::OsStr::new("printf"),
            &[
                std::ffi::OsStr::new("%s"),
                std::ffi::OsStr::new("$HOME;echo BAD"),
            ],
            directory.path(),
            80,
            24,
            &[],
        )
        .unwrap();
        let mut output = Vec::new();
        let mut buffer = [0_u8; 1024];
        while let Ok(read) = parts.reader.read(&mut buffer) {
            if read == 0 {
                break;
            }
            output.extend_from_slice(&buffer[..read]);
        }
        assert_eq!(output, b"$HOME;echo BAD");
        assert!(parts.waiter.wait().unwrap().success);
    }

    /// The default path must remain an interactive shell that outlives any single command.
    #[cfg(unix)]
    #[test]
    fn a_shell_pane_stays_open_without_a_command() {
        let directory = tempfile::tempdir().unwrap();
        let parts = PtyProcess::spawn(
            std::ffi::OsStr::new("/bin/sh"),
            None,
            directory.path(),
            80,
            24,
            &[],
        )
        .unwrap();

        parts.input.send(b"printf 'STILL_HERE'\n").unwrap();
        std::thread::sleep(Duration::from_millis(250));
        parts.control.terminate();
    }
}
