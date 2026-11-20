//! Server-owned original grants, separate from foreground write acknowledgements.

// Rust guideline compliant 2026-10-07
use crate::ipc::{self, SharedWriter};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    io,
    sync::Arc,
};
use vivid_protocol::overlay::wire::PresentationOutcome;
use vivid_sdk::presenter::VirtualVivid;
use vivid_sdk::presenter::{
    BridgeSourceKey, OverlayPresentationAttempt, OverlayPresentationCapture,
    OverlayPresentationReceipt, OverlayPresentationRoute,
};

// SDK admission is four copies per owner and 64 globally. Pending byte storage
// additionally shares the configured media IPC bound; the actor never waits.
const CAPACITY: usize = 64;
struct Copy {
    client: u64,
    bridge: Option<u64>,
    source: BridgeSourceKey,
    projection: u64,
    receipt: OverlayPresentationReceipt,
    attempt: Arc<OverlayPresentationAttempt>,
}
struct Handoff {
    source: BridgeSourceKey,
    delivery: u64,
    projection: u64,
    body: Arc<[u8]>,
    attempt: Arc<OverlayPresentationAttempt>,
}
struct Seen {
    attempt: Arc<OverlayPresentationAttempt>,
    projection: u64,
}
pub(super) struct NativePresentations {
    route: OverlayPresentationRoute,
    next: u64,
    copies: HashMap<u64, Copy>,
    pending: VecDeque<Handoff>,
    bytes: usize,
    byte_limit: usize,
    seen: HashMap<BridgeSourceKey, Seen>,
    replay: HashSet<BridgeSourceKey>,
}
impl NativePresentations {
    pub(super) fn new(route: OverlayPresentationRoute, byte_limit: usize) -> Self {
        Self {
            route,
            next: 0,
            copies: HashMap::new(),
            pending: VecDeque::new(),
            bytes: 0,
            byte_limit,
            seen: HashMap::new(),
            replay: HashSet::new(),
        }
    }
    pub(super) fn submit(
        &mut self,
        vivid: &VirtualVivid,
        delivery: u64,
        source: BridgeSourceKey,
        projection: u64,
        body: Arc<[u8]>,
    ) {
        let attempt = body.get(..12).and_then(|header| {
            let epoch = u32::from_be_bytes(header[..4].try_into().ok()?);
            let revision = u64::from_be_bytes(header[4..].try_into().ok()?);
            self.route
                .begin_capture(source, epoch, revision)
                .ok()
                .flatten()
        });
        let Some(attempt) = attempt else {
            vivid.release_bridge_delivery(delivery);
            return;
        };
        let attempt = Arc::new(attempt);
        if self
            .seen
            .get(&source)
            .is_some_and(|seen| seen.attempt.same_original(&attempt))
        {
            vivid.release_bridge_delivery(delivery);
            return;
        }
        if let Some(pending) = self
            .pending
            .iter_mut()
            .find(|pending| pending.source == source && pending.attempt.same_original(&attempt))
        {
            // A retained replay may beat the live event into this queue. Keep
            // that event's flow allowance until this copy is forwarded, so later
            // asset operations cannot overtake a frame waiting for copy credit.
            if pending.delivery == 0 || !vivid.bridge_delivery_is_pending(pending.delivery, source)
            {
                pending.delivery = delivery;
            } else {
                vivid.release_bridge_delivery(delivery);
            }
            return;
        }
        // Coalesce only copies not yet forwarded, after pinning their opaque origin.
        if let Some(index) = self
            .pending
            .iter()
            .position(|pending| pending.source == source)
        {
            let old = self.pending.remove(index).expect("located handoff");
            self.bytes -= old.body.len();
            vivid.release_bridge_delivery(old.delivery);
        }
        if self.pending.len() >= CAPACITY || body.len() > self.byte_limit.saturating_sub(self.bytes)
        {
            vivid.release_bridge_delivery(delivery);
            self.replay.insert(source);
            return;
        }
        self.bytes += body.len();
        self.pending.push_back(Handoff {
            source,
            delivery,
            projection,
            body,
            attempt,
        });
    }
    pub(super) fn progress(
        &mut self,
        vivid: &VirtualVivid,
        attachment: Option<(u64, Option<u64>, SharedWriter)>,
    ) {
        for _ in 0..self.pending.len().min(super::MEDIA_EVENTS_PER_TURN) {
            let handoff = self.pending.pop_front().expect("bounded pending handoff");
            self.bytes -= handoff.body.len();
            if vivid.pane_for_source(handoff.source).is_none()
                || (handoff.delivery != 0
                    && !vivid.bridge_delivery_is_pending(handoff.delivery, handoff.source))
            {
                vivid.release_bridge_delivery(handoff.delivery);
                continue;
            }
            let Some((client, bridge, writer)) = &attachment else {
                vivid.release_bridge_delivery(handoff.delivery);
                self.replay.insert(handoff.source);
                continue;
            };
            if self.copies.len() >= CAPACITY {
                self.bytes += handoff.body.len();
                self.pending.push_back(handoff);
                continue;
            }
            let capture = handoff.attempt.capture();
            let receipt = match capture {
                Ok(OverlayPresentationCapture::Forwarded(receipt)) => Some(receipt),
                Ok(OverlayPresentationCapture::Replay) => None,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    self.bytes += handoff.body.len();
                    self.pending.push_back(handoff);
                    continue;
                }
                _ => {
                    vivid.release_bridge_delivery(handoff.delivery);
                    continue;
                }
            };
            let correlation = if receipt.is_some() {
                let Some(next) = self.next.checked_add(1) else {
                    if let Some(receipt) = receipt {
                        receipt.abandon();
                    }
                    vivid.release_bridge_delivery(handoff.delivery);
                    continue;
                };
                self.next = next;
                next
            } else {
                0
            };
            if ipc::send_media_record_with_presentation(
                writer,
                handoff.delivery,
                correlation,
                handoff.source,
                vivid_protocol::messages::VECTOR_FRAME,
                &handoff.body,
            )
            .is_err()
            {
                if let Some(receipt) = receipt {
                    receipt.abandon();
                }
                vivid.release_bridge_delivery(handoff.delivery);
                self.replay.insert(handoff.source);
                continue;
            }
            if let Some(receipt) = receipt {
                self.copies.insert(
                    correlation,
                    Copy {
                        client: *client,
                        bridge: *bridge,
                        source: handoff.source,
                        projection: handoff.projection,
                        receipt,
                        attempt: Arc::clone(&handoff.attempt),
                    },
                );
            }
            self.seen.insert(
                handoff.source,
                Seen {
                    attempt: handoff.attempt,
                    projection: handoff.projection,
                },
            );
        }
    }
    pub(super) fn report(
        &mut self,
        client: u64,
        bridge: u64,
        correlation: u64,
        source: BridgeSourceKey,
        outcome: Option<u8>,
    ) {
        if !matches!(outcome, None | Some(0 | 1))
            || !self.copies.get(&correlation).is_some_and(|copy| {
                copy.client == client && copy.bridge == Some(bridge) && copy.source == source
            })
        {
            return;
        }
        let copy = self
            .copies
            .remove(&correlation)
            .expect("validated original copy");
        match outcome {
            Some(0) => {
                copy.receipt.complete(PresentationOutcome::Presented);
            }
            Some(1) => {
                copy.receipt.complete(PresentationOutcome::Superseded);
            }
            _ => {
                copy.receipt.abandon();
                if self
                    .seen
                    .get(&source)
                    .is_some_and(|seen| seen.attempt.same_original(&copy.attempt))
                {
                    self.seen.remove(&source);
                    self.replay.insert(source);
                }
            }
        }
    }
    pub(super) fn bind(&mut self, client: u64, bridge: u64) {
        for copy in self
            .copies
            .values_mut()
            .filter(|copy| copy.client == client && copy.bridge.is_none())
        {
            copy.bridge = Some(bridge);
        }
    }
    pub(super) fn reset(&mut self, vivid: &VirtualVivid) {
        for (_, copy) in self.copies.drain() {
            copy.receipt.abandon();
            self.replay.insert(copy.source);
        }
        for handoff in self.pending.drain(..) {
            vivid.release_bridge_delivery(handoff.delivery);
            self.replay.insert(handoff.source);
        }
        self.bytes = 0;
        self.replay.extend(self.seen.keys().copied());
        self.seen.clear();
    }
    pub(super) fn reconcile(&mut self, vivid: &VirtualVivid, sources: &HashSet<BridgeSourceKey>) {
        let retired = self
            .copies
            .iter()
            .filter(|(_, copy)| !sources.contains(&copy.source))
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        for id in retired {
            self.copies
                .remove(&id)
                .expect("retired copy")
                .receipt
                .abandon();
        }
        self.pending.retain(|handoff| {
            if sources.contains(&handoff.source) {
                true
            } else {
                self.bytes -= handoff.body.len();
                vivid.release_bridge_delivery(handoff.delivery);
                false
            }
        });
        self.seen.retain(|source, _| sources.contains(source));
        self.replay
            .retain(|source| vivid.pane_for_source(*source).is_some());
    }
    pub(super) fn recreated(&mut self, sources: &[BridgeSourceKey], projection: u64) {
        for source in sources {
            let retired = self
                .copies
                .iter()
                .filter(|(_, copy)| copy.source == *source && copy.projection < projection)
                .map(|(id, _)| *id)
                .collect::<Vec<_>>();
            for id in retired {
                self.copies
                    .remove(&id)
                    .expect("recreated copy")
                    .receipt
                    .abandon();
            }
            if self
                .seen
                .get(source)
                .is_some_and(|seen| seen.projection < projection)
            {
                self.seen.remove(source);
                self.replay.insert(*source);
            }
        }
    }
    pub(super) fn take_replay(&mut self) -> HashSet<BridgeSourceKey> {
        std::mem::take(&mut self.replay)
    }
}

impl super::SessionActor {
    pub(super) fn submit_native_frame(
        &mut self,
        delivery: u64,
        source: BridgeSourceKey,
        body: Arc<[u8]>,
    ) {
        self.native_presentations.submit(
            &self.vivid,
            delivery,
            source,
            self.media_projection_revision,
            body,
        );
        self.progress_native_presentations();
    }
    fn progress_native_presentations(&mut self) {
        let attachment = self
            .presenter_client()
            .filter(|client| client.vivid)
            .map(|client| {
                (
                    client.id,
                    self.bridge_instance_id,
                    Arc::clone(&client.writer),
                )
            });
        self.native_presentations.progress(&self.vivid, attachment);
    }
    pub(super) fn service_native_presentations(&mut self) {
        self.progress_native_presentations();
        if self.presenter_client().is_some_and(|client| client.vivid) {
            let replay = self.native_presentations.take_replay();
            if !replay.is_empty() {
                self.retained_replay_requests.extend(replay);
                self.last_media_projection = None;
                self.media_projection_pending
                    .store(true, std::sync::atomic::Ordering::Release);
            }
        }
    }
    pub(super) fn bind_native_presentations(&mut self, client: u64, bridge: u64) {
        self.window_actions.bind(&self.vivid, client, bridge);
        if self.bridge_instance_id.is_some_and(|old| old != bridge) {
            self.documents.reset(&self.vivid);
            self.native_presentations.reset(&self.vivid);
        }
        self.native_presentations.bind(client, bridge);
    }
}

#[cfg(all(test, unix))]
#[path = "presentations_tests.rs"]
mod tests;
