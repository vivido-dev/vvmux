//! Bounded original editor grants retained by the session actor across publication replies.

// Rust guideline compliant 2026-10-07
use std::{
    collections::{HashMap, HashSet, VecDeque},
    io,
};
use vivid_protocol::{cbor::Value, messages, overlay::wire::text::editor::SetContext};
use vivid_sdk::presenter::{
    BridgeSurfaceKey, OverlayDocumentTarget, OverlayHostRequest, VirtualVivid,
};

use crate::document_input::DocumentInput;

struct Publication {
    id: u64,
    editor: u64,
    revision: u64,
    target: OverlayDocumentTarget,
}

#[derive(Default)]
pub(super) struct Documents {
    windows: HashMap<BridgeSurfaceKey, VecDeque<Publication>>,
    last_sequence: u64,
    pending: HashSet<u64>,
}

impl Documents {
    pub(super) fn publish(
        &mut self,
        inner: &VirtualVivid,
        request: &OverlayHostRequest,
    ) -> io::Result<()> {
        let retired: Vec<_> = self
            .pending
            .iter()
            .copied()
            .filter(|id| !inner.overlay_host_request_pending(*id))
            .collect();
        for id in retired {
            self.replied(id, false);
        }
        if self.pending.len() >= 64 {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        let key = request.surface;
        let envelope = messages::decode_control(&request.body)?;
        let query = SetContext::decode(key.surface, &Value::Map(envelope.payload))?;
        let Some(context) = query.context else {
            self.windows.remove(&key);
            self.pending.insert(request.id);
            return Ok(());
        };
        let target = inner
            .take_overlay_document_target(request.id, key)
            .ok_or_else(|| io::Error::other("original document publication expired"))?;
        if self.windows.len() >= vivid_protocol::overlay::MAX_WINDOWS_PER_OWNER
            && !self.windows.contains_key(&key)
        {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        let revisions = self.windows.entry(key).or_default();
        if revisions.len() == 64 {
            revisions.pop_front();
        }
        self.pending.insert(request.id);
        revisions.push_back(Publication {
            id: request.id,
            editor: context.editor,
            revision: context.revision,
            target,
        });
        Ok(())
    }

    pub(super) fn reset(&mut self, inner: &VirtualVivid) {
        for id in self.pending.drain() {
            inner.complete_overlay_host_request(id, Err("document attachment retired".into()));
        }
        self.windows.clear();
        self.last_sequence = 0;
    }

    pub(super) fn replied(&mut self, id: u64, accepted: bool) {
        if !self.pending.remove(&id) || accepted {
            return;
        }
        self.windows.retain(|_, revisions| {
            revisions.retain(|publication| publication.id != id);
            !revisions.is_empty()
        });
    }

    pub(super) fn reconcile(&mut self, surfaces: &[vivid_sdk::presenter::BridgeSurface]) {
        self.windows.retain(|key, _| {
            surfaces
                .iter()
                .any(|s| s.key == *key && s.overlay_window.is_some())
        });
    }

    pub(super) fn forward(
        &mut self,
        inner: &VirtualVivid,
        input: DocumentInput,
    ) -> io::Result<bool> {
        if input.sequence <= self.last_sequence {
            return Ok(false);
        }
        self.last_sequence = input.sequence;
        if self.pending.contains(&input.publication)
            && !inner.overlay_host_request_pending(input.publication)
        {
            self.replied(input.publication, false);
            return Ok(false);
        }
        let Some(publication) = self
            .windows
            .get(&input.surface)
            .and_then(|revisions| revisions.iter().find(|p| p.id == input.publication))
        else {
            return Ok(false);
        };
        let Some((edit, deadline)) = input.decode()? else {
            return Ok(false);
        };
        if edit.editor != publication.editor || edit.revision != publication.revision {
            return Ok(false);
        }
        inner.relay_overlay_document_edit_before(&publication.target, &edit, deadline)
    }
}
