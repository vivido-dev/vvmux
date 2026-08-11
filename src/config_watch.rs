//! A polling watcher for the session's config file.
//!
//! Polling rather than an OS notification API: a config file changes at human speed, one `stat`
//! per second is unmeasurable next to the render loop, and it keeps a filesystem-notification
//! dependency out of a crate that otherwise has none. Polling the *path* also handles the way
//! editors actually save — write-to-temp then rename — which invalidates an inode-based watch.
//!
//! Two rules keep the watcher from acting on a half-written file or an absent one:
//!
//! - A change must be observed twice with the same stamp before it fires. An editor that
//!   truncates and then writes would otherwise be read mid-write.
//! - A file that disappears is not a change. Deleting a config must never silently reset a live
//!   session to defaults; the last good config stays in force.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, SystemTime};

use crate::session::ActorEvent;

/// How often the file is stat'd. Also the debounce interval, since a change must survive one
/// further poll before it fires.
#[cfg(not(test))]
const POLL_INTERVAL: Duration = Duration::from_secs(1);
#[cfg(test)]
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// What identifies a revision of the file. Modification time alone misses a same-second edit that
/// changes the length, and length alone misses an in-place edit of the same size.
type Stamp = (Option<SystemTime>, u64);

fn stamp(path: &std::path::Path) -> Option<Stamp> {
    let metadata = std::fs::metadata(path).ok()?;
    Some((metadata.modified().ok(), metadata.len()))
}

/// Watch `path`, waking the actor whenever it settles on new contents.
///
/// The wake is coalesced through `pending` exactly like the media wakeup: a wake already queued
/// and not yet observed makes another redundant, and a dropped wake is harmless because the actor
/// re-reads the file itself when it handles the event.
pub fn spawn(
    path: PathBuf,
    sender: mpsc::SyncSender<ActorEvent>,
    shutdown: Arc<AtomicBool>,
    pending: Arc<AtomicBool>,
) -> std::io::Result<()> {
    spawn_path(
        path,
        sender,
        shutdown,
        pending,
        "vvmux-config-watch",
        || ActorEvent::ConfigChanged,
    )
}

/// Watch the global plugin registry for a settled atomic replacement.
///
/// Plugin removal is represented by a newer registry generation without that entry; the registry
/// file itself is durable and its deletion is ignored so generation replay protection cannot be
/// reset accidentally. The same coalesced wakeup rule keeps this watcher from adding actor work
/// when nothing changes.
pub fn spawn_plugin_registry(
    path: PathBuf,
    sender: mpsc::SyncSender<ActorEvent>,
    shutdown: Arc<AtomicBool>,
    pending: Arc<AtomicBool>,
) -> std::io::Result<()> {
    spawn_path(
        path,
        sender,
        shutdown,
        pending,
        "vvmux-plugin-registry-watch",
        || ActorEvent::PluginsChanged,
    )
}

fn spawn_path(
    path: PathBuf,
    sender: mpsc::SyncSender<ActorEvent>,
    shutdown: Arc<AtomicBool>,
    pending: Arc<AtomicBool>,
    thread_name: &'static str,
    event: fn() -> ActorEvent,
) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name(thread_name.into())
        .spawn(move || {
            let mut current = stamp(&path);
            // A change seen once but not yet confirmed by a second identical observation.
            let mut settling: Option<Stamp> = None;
            while !shutdown.load(Ordering::Acquire) {
                std::thread::sleep(POLL_INTERVAL);
                if shutdown.load(Ordering::Acquire) {
                    break;
                }
                let observed = stamp(&path);
                // A file that is gone, or briefly gone mid-rename, is not a change.
                let Some(observed) = observed else {
                    settling = None;
                    continue;
                };
                if Some(observed) == current {
                    settling = None;
                    continue;
                }
                match settling {
                    // Second identical observation: the writer has stopped.
                    Some(previous) if previous == observed => {
                        current = Some(observed);
                        settling = None;
                        if !pending.swap(true, Ordering::AcqRel)
                            && sender.try_send(event()).is_err()
                        {
                            // The queue is full or the actor is gone. Clear the bit so the next
                            // poll retries rather than latching pending forever.
                            pending.store(false, Ordering::Release);
                        }
                    }
                    _ => settling = Some(observed),
                }
            }
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stamp_distinguishes_a_same_size_edit_from_no_edit() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::write(&path, "a = 1").unwrap();

        let first = stamp(&path).unwrap();
        assert_eq!(stamp(&path).unwrap(), first, "an untouched file is stable");

        // Same length, different bytes: only the modification time can tell these apart, so a
        // length-only stamp would miss this edit entirely.
        std::thread::sleep(Duration::from_millis(10));
        std::fs::write(&path, "a = 2").unwrap();
        let second = stamp(&path).unwrap();
        assert_eq!(second.1, first.1, "the length is unchanged by construction");
        assert_ne!(second, first, "the stamp must still register the edit");
    }

    #[test]
    fn a_missing_file_has_no_stamp() {
        let directory = tempfile::tempdir().unwrap();
        assert!(stamp(&directory.path().join("absent.toml")).is_none());
    }

    #[test]
    fn the_watcher_stops_when_the_session_shuts_down() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::write(&path, "").unwrap();
        let (sender, receiver) = mpsc::sync_channel(4);
        let shutdown = Arc::new(AtomicBool::new(true));

        spawn(path, sender, shutdown, Arc::new(AtomicBool::new(false))).unwrap();

        // The thread observes the shutdown flag and exits, dropping its sender.
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(5)),
            Err(mpsc::RecvTimeoutError::Disconnected)
        ));
    }

    #[test]
    fn an_unchanged_plugin_registry_adds_no_actor_wakeup() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("registry.json");
        std::fs::write(&path, r#"{"schema":1,"generation":1,"plugins":{}}"#).unwrap();
        let (sender, receiver) = mpsc::sync_channel(4);
        let shutdown = Arc::new(AtomicBool::new(false));
        spawn_plugin_registry(
            path,
            sender,
            shutdown.clone(),
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        assert!(matches!(
            receiver.recv_timeout(POLL_INTERVAL * 5),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        shutdown.store(true, Ordering::Release);
    }

    #[test]
    fn plugin_registry_wakeups_are_settled_and_coalesced() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("registry.json");
        std::fs::write(&path, r#"{"schema":1,"generation":1,"plugins":{}}"#).unwrap();
        let (sender, receiver) = mpsc::sync_channel(4);
        let shutdown = Arc::new(AtomicBool::new(false));
        let pending = Arc::new(AtomicBool::new(false));
        spawn_plugin_registry(path.clone(), sender, shutdown.clone(), pending.clone()).unwrap();
        std::thread::sleep(POLL_INTERVAL * 2);
        std::fs::write(&path, r#"{"schema":1,"generation":2,"plugins":{}}"#).unwrap();
        assert!(matches!(
            receiver.recv_timeout(POLL_INTERVAL * 5),
            Ok(ActorEvent::PluginsChanged)
        ));
        std::fs::write(&path, r#"{"schema":1,"generation":3,"plugins":{}}"#).unwrap();
        assert!(matches!(
            receiver.recv_timeout(POLL_INTERVAL * 5),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        pending.store(false, Ordering::Release);
        std::fs::write(&path, r#"{"schema":1,"generation":4,"plugins":{}}"#).unwrap();
        assert!(matches!(
            receiver.recv_timeout(POLL_INTERVAL * 5),
            Ok(ActorEvent::PluginsChanged)
        ));
        shutdown.store(true, Ordering::Release);
    }
}
