//! Original SDK window-action grants never leave the session actor.

// Rust guideline compliant 2026-10-07
use super::{ServerMessage, SessionActor};
use std::{collections::HashMap, sync::Arc};
use vivid_sdk::presenter::{
    OverlayActionRequest, OverlayActionRoute, OverlayHostRequest, VirtualVivid,
};

// Match the SDK's global host-request ceiling; each owner is independently bounded there.
const CAPACITY: usize = 64;
struct Pending {
    client: u64,
    bridge: Option<u64>,
    request: OverlayHostRequest,
    receipt: OverlayActionRequest,
    claimed: bool,
}
pub(super) struct WindowActions {
    pub(super) route: OverlayActionRoute,
    binding: Option<(u64, u64)>,
    pending: HashMap<u64, Pending>,
}
impl WindowActions {
    pub(super) fn new(route: OverlayActionRoute) -> Self {
        Self {
            route,
            binding: None,
            pending: HashMap::new(),
        }
    }
    pub(super) fn reset(&mut self, vivid: &VirtualVivid) {
        self.route.retire_pending();
        for id in self.pending.keys() {
            vivid.complete_overlay_host_request(
                *id,
                Err("window action attachment was retired".into()),
            );
        }
        self.pending.clear();
        self.binding = None;
    }
    pub(super) fn bind(&mut self, vivid: &VirtualVivid, client: u64, bridge: u64) {
        if self.binding.is_some_and(|old| old != (client, bridge)) {
            self.reset(vivid);
        }
        self.binding = Some((client, bridge));
    }
    pub(super) fn contains(&self, id: u64) -> bool {
        self.pending.contains_key(&id)
    }
    pub(super) fn pending_counts(&self) -> (usize, usize) {
        (
            self.pending.len(),
            self.pending
                .values()
                .filter(|pending| pending.claimed)
                .count(),
        )
    }
    pub(super) fn enqueue(
        &mut self,
        vivid: &VirtualVivid,
        client: u64,
        request: OverlayHostRequest,
    ) {
        let receipt = self.route.request(&request);
        if request.id == 0
            || request.record_type != vivid_protocol::messages::OVERLAY_ACTION
            || request.body.len() > vivid_protocol::LANE_MAX_RECORD_BODY as usize
            || self.pending.len() >= CAPACITY
            || !receipt.is_pending()
        {
            vivid.complete_overlay_host_request(
                request.id,
                Err("original window action is unavailable".into()),
            );
            return;
        }
        self.pending.entry(request.id).or_insert(Pending {
            client,
            bridge: None,
            request,
            receipt,
            claimed: false,
        });
    }
    pub(super) fn service(
        &mut self,
        vivid: &VirtualVivid,
        client: Option<(u64, u64, crate::ipc::SharedWriter)>,
    ) {
        let mut retired = Vec::new();
        for (id, pending) in &mut self.pending {
            let current = client.as_ref().filter(|(owner, bridge, _)| {
                *owner == pending.client && pending.bridge.is_none_or(|old| old == *bridge)
            });
            if pending.receipt.remaining_lifetime().is_none()
                || (!pending.claimed && !pending.receipt.is_pending())
                || (pending.bridge.is_some() && current.is_none())
            {
                if let Some((_, bridge, writer)) = &client
                    && pending.bridge == Some(*bridge)
                {
                    let _ = crate::ipc::send(
                        writer,
                        &ServerMessage::WindowActionRetired {
                            bridge_instance_id: *bridge,
                            id: *id,
                        },
                    );
                }
                retired.push(*id);
                continue;
            }
            if pending.bridge.is_some() {
                continue;
            }
            let Some((_, bridge, writer)) = current else {
                continue;
            };
            let Some(windows) = pending.receipt.preceding_windows() else {
                retired.push(*id);
                continue;
            };
            let centered_bounds = pending.receipt.centered_bounds().map(|r| {
                [
                    r.origin.x.raw(),
                    r.origin.y.raw(),
                    r.width.raw(),
                    r.height.raw(),
                ]
            });
            let message = crate::ipc::WindowActionRequest {
                bridge_instance_id: *bridge,
                request: pending.request.clone(),
                preceding_windows: windows
                    .into_iter()
                    .map(|w| (w.surface, w.generation, w.revision))
                    .collect(),
                centered_bounds,
            };
            if crate::ipc::send(writer, &ServerMessage::WindowActionRequest(message)).is_ok() {
                pending.bridge = Some(*bridge);
            } else {
                retired.push(*id);
            }
        }
        for id in retired {
            self.pending.remove(&id);
            vivid.complete_overlay_host_request(
                id,
                Err("original window action was retired or expired".into()),
            );
        }
    }
    pub(super) fn claim(&mut self, client: u64, bridge: u64, id: u64) -> (bool, u64) {
        let Some(pending) = self.pending.get_mut(&id) else {
            return (false, 0);
        };
        if pending.client != client || pending.bridge != Some(bridge) || pending.claimed {
            return (false, 0);
        }
        if !self.route.request(&pending.request).claim() {
            return (false, 0);
        }
        pending.claimed = true;
        let remaining = pending
            .receipt
            .remaining_lifetime()
            .and_then(|duration| u64::try_from(duration.as_micros()).ok())
            .unwrap_or(0);
        (remaining != 0, remaining)
    }
    pub(super) fn reply(
        &mut self,
        vivid: &VirtualVivid,
        client: u64,
        bridge: u64,
        id: u64,
        response: Result<Vec<u8>, String>,
    ) -> bool {
        let Some(pending) = self.pending.get(&id) else {
            return false;
        };
        if pending.client != client || pending.bridge != Some(bridge) {
            return false;
        }
        let response = if response.is_ok() && !pending.claimed {
            Err("window action was not admitted".into())
        } else {
            response
        };
        self.pending.remove(&id);
        vivid.complete_overlay_host_request(id, response);
        true
    }
}

impl SessionActor {
    pub(super) fn service_window_actions(&mut self) {
        let client = self
            .presenter_client()
            .filter(|client| client.vivid)
            .and_then(|client| {
                self.bridge_instance_id
                    .map(|bridge| (client.id, bridge, Arc::clone(&client.writer)))
            });
        self.window_actions.service(&self.vivid, client);
    }
}

#[cfg(all(test, unix))]
#[path = "window_actions_tests.rs"]
mod tests;
