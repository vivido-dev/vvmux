//! Bounded foreground window actions and one-shot original-server decisions.

// Rust guideline compliant 2026-10-07
use super::{BridgeClientSender, BridgeWorker, ClientMessage, TrackMediaQueues};
use crate::ipc::WindowActionRequest;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    io,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use vivid_gateway::ExternalActionClaim;
use vivid_protocol::vector::{Rect, Scalar};
use vivid_sdk::presenter::OverlayActionWindow;

type Key = (u64, u64);
const CAPACITY: usize = 64;
// A local queue bound never replaces the server's authoritative original deadline.
const LIFETIME: Duration = Duration::from_secs(10);
struct Queued {
    request: WindowActionRequest,
    received: Instant,
}
#[derive(Default)]
pub(super) struct Queue {
    requests: VecDeque<Queued>,
    active: HashSet<Key>,
    decisions: HashMap<Key, (bool, u64)>,
    retired: HashSet<Key>,
}
impl Queue {
    pub(super) fn request(&mut self, request: WindowActionRequest) -> bool {
        if request.bridge_instance_id == 0
            || request.request.id == 0
            || request.request.record_type != vivid_protocol::messages::OVERLAY_ACTION
            || request.request.body.len() > vivid_protocol::LANE_MAX_RECORD_BODY as usize
            || request.preceding_windows.len() > vivid_protocol::overlay::MAX_WINDOWS_PER_OWNER
            || self.requests.len() >= CAPACITY
        {
            return false;
        }
        self.requests.push_back(Queued {
            request,
            received: Instant::now(),
        });
        true
    }
    pub(super) fn decision(&mut self, bridge: u64, id: u64, allowed: bool, remaining: u64) {
        if self.active.contains(&(bridge, id)) {
            self.decisions
                .entry((bridge, id))
                .or_insert((allowed, remaining));
        }
    }
    pub(super) fn retire(&mut self, bridge: u64, id: u64) -> bool {
        self.requests.retain(|queued| {
            queued.request.bridge_instance_id != bridge || queued.request.request.id != id
        });
        if self.active.contains(&(bridge, id)) {
            if self.retired.len() >= CAPACITY {
                return false;
            }
            self.retired.insert((bridge, id));
        }
        true
    }
}
struct Waiting {
    receipt: ExternalActionClaim,
    sent: Instant,
}
#[derive(Default)]
pub(super) struct Actions {
    active: HashMap<u64, u64>,
    waiting: HashMap<Key, Waiting>,
}
impl Actions {
    pub(super) fn reply(&mut self, queues: &Arc<Mutex<TrackMediaQueues>>, id: u64) -> Option<u64> {
        let bridge = self.active.remove(&id)?;
        self.waiting.remove(&(bridge, id));
        let mut queues = queues
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        queues.window_actions.active.remove(&(bridge, id));
        queues.window_actions.decisions.remove(&(bridge, id));
        queues.window_actions.retired.remove(&(bridge, id));
        Some(bridge)
    }
    pub(super) fn poll(
        &mut self,
        bridge: &mut vivid_gateway::OuterBridge,
        queues: &Arc<Mutex<TrackMediaQueues>>,
        sender: &BridgeClientSender,
        current: u64,
    ) -> io::Result<()> {
        let (decisions, mut retired) = {
            let mut queues = queues
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (
                std::mem::take(&mut queues.window_actions.decisions),
                std::mem::take(&mut queues.window_actions.retired),
            )
        };
        retired.extend(
            self.active
                .iter()
                .filter(|(_, binding)| **binding != current)
                .map(|(id, binding)| (*binding, *id)),
        );
        for key in retired {
            if self.active.get(&key.1) == Some(&key.0) {
                bridge.retire_external_overlay_action(key.1);
                self.waiting.remove(&key);
            }
        }
        for (key, (allowed, remaining_us)) in decisions {
            if key.0 != current {
                continue;
            }
            if let Some(waiting) = self.waiting.remove(&key) {
                // The full RTT conservatively overestimates time spent after the
                // server's claim. Arrival must never start a fresh source lifetime.
                let remaining = Duration::from_micros(remaining_us);
                let deadline = waiting.sent.checked_add(remaining).unwrap_or(waiting.sent);
                let _ = waiting
                    .receipt
                    .complete_before(allowed && remaining <= LIFETIME, deadline);
            }
        }
        for receipt in bridge.take_external_overlay_action_claims() {
            let id = receipt.request_id();
            if self.active.get(&id) != Some(&current) {
                drop(receipt);
                continue;
            }
            let key = (current, id);
            if self.waiting.contains_key(&key) {
                drop(receipt);
                continue;
            }
            self.waiting.insert(
                key,
                Waiting {
                    receipt,
                    sent: Instant::now(),
                },
            );
            if let Err(error) = sender.send(ClientMessage::WindowActionClaim {
                bridge_instance_id: current,
                id,
            }) {
                self.waiting.remove(&key);
                return Err(error);
            }
        }
        let count = queues
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .window_actions
            .requests
            .len();
        for _ in 0..count {
            let Some(queued) = queues
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .window_actions
                .requests
                .pop_front()
            else {
                break;
            };
            let wire = &queued.request;
            let id = wire.request.id;
            if wire.bridge_instance_id != current || queued.received.elapsed() >= LIFETIME {
                sender.send(ClientMessage::WindowActionReply {
                    bridge_instance_id: wire.bridge_instance_id,
                    id,
                    response: Err("window action route was retired or expired".into()),
                })?;
                continue;
            }
            let preceding = wire
                .preceding_windows
                .iter()
                .map(|(surface, generation, revision)| OverlayActionWindow {
                    surface: *surface,
                    generation: *generation,
                    revision: *revision,
                })
                .collect::<Vec<_>>();
            let centered = wire.centered_bounds.map(decode_bounds).transpose();
            let result = centered.and_then(|centered| {
                bridge.start_external_overlay_action(&wire.request, &preceding, centered)
            });
            match result {
                Ok(()) => {
                    self.active.insert(id, current);
                    queues
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .window_actions
                        .active
                        .insert((current, id));
                    break;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    queues
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .window_actions
                        .requests
                        .push_back(queued);
                }
                Err(error) => sender.send(ClientMessage::WindowActionReply {
                    bridge_instance_id: current,
                    id,
                    response: Err(error.to_string()),
                })?,
            }
        }
        Ok(())
    }
}
fn decode_bounds(bounds: [i64; 4]) -> io::Result<Rect> {
    let scalars = bounds.map(Scalar::from_raw);
    let [x, y, width, height] = scalars;
    Rect::new(
        x.map_err(io::Error::other)?.get(),
        y.map_err(io::Error::other)?.get(),
        width.map_err(io::Error::other)?.get(),
        height.map_err(io::Error::other)?.get(),
    )
    .map_err(io::Error::other)
}

impl BridgeWorker {
    pub(crate) fn queue_window_action(&mut self, request: WindowActionRequest) {
        if !self.media.lock().unwrap().window_actions.request(request) {
            (self.cancel)();
        }
        if let Some(wakeup) = &self.media_wakeup {
            let _ = wakeup.try_send(());
        }
    }
    pub(crate) fn queue_window_action_decision(
        &mut self,
        bridge: u64,
        id: u64,
        allowed: bool,
        remaining_us: u64,
    ) {
        self.media
            .lock()
            .unwrap()
            .window_actions
            .decision(bridge, id, allowed, remaining_us);
        if let Some(wakeup) = &self.media_wakeup {
            let _ = wakeup.try_send(());
        }
    }
    pub(crate) fn retire_window_action(&mut self, bridge: u64, id: u64) {
        if !self.media.lock().unwrap().window_actions.retire(bridge, id) {
            (self.cancel)();
        }
        if let Some(wakeup) = &self.media_wakeup {
            let _ = wakeup.try_send(());
        }
    }
}
