#![cfg(unix)]

//! A detached session server must not keep descriptors it was never given.
//!
//! The daemon outlives the launcher that forked it, so anything it inherits by accident is held for
//! the session's whole life. That is how concurrent fixtures used to deadlock: `Command` opens its
//! capture pipes and marks them close-on-exec in two steps, so a launch racing in another thread
//! forks with an unrelated pipe still inheritable, and the daemon then holds that pipe's writing
//! end forever while the thread waiting on `Command::output` never sees EOF. This reproduces the
//! inheritance deliberately instead of racing for it: an inheritable pipe is handed to the
//! launcher, and once the test drops its own end the reader must reach EOF.

use crate::common;

use std::io::Read;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::sync::mpsc;
use std::time::Duration;

/// Long enough that a slow machine cannot fail this, short enough to stay a test.
const EOF_TIMEOUT: Duration = Duration::from_secs(10);

#[test]
fn a_detached_server_does_not_inherit_its_launchers_descriptors() {
    let directory = tempfile::Builder::new()
        .prefix("vvfd-")
        .tempdir_in("/tmp")
        .unwrap();
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();

    let (mut reader, writer) = std::io::pipe().unwrap();
    let writer = OwnedFd::from(writer);
    let inherited = writer.as_raw_fd();

    let name = format!("fd-{}", std::process::id());
    let mut command = common::vvmux_command(directory.path());
    command.args(["new", "-s", &name, "-d"]);
    // Clear close-on-exec in the forked child rather than here. The descriptor table is shared with
    // every other test in this binary until the fork, so an inheritable writing end in this process
    // would also reach the long-lived children those tests spawn, and they — not the daemon under
    // test — would be what holds the pipe open.
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(inherited, libc::F_SETFD, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let created = command.output().unwrap();
    assert!(
        created.status.success(),
        "new -d failed: {}",
        String::from_utf8_lossy(&created.stderr)
    );

    // The launcher has exited, so from here only a process that inherited the writing end can keep
    // the pipe open. Drop the test's own copy and the read must end.
    drop(writer);
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut leftovers = Vec::new();
        let _ = sender.send(reader.read_to_end(&mut leftovers));
    });
    let reached_eof = receiver.recv_timeout(EOF_TIMEOUT);

    let _ = common::vvmux_command(directory.path())
        .args(["kill-session", "--target", &name])
        .output();

    assert!(
        matches!(reached_eof, Ok(Ok(0))),
        "the detached server kept an inherited descriptor open: {reached_eof:?}"
    );
}
