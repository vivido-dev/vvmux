//! Server-retained SDK input stamps; IPC exposes only non-authoritative correlations.

// Rust guideline compliant 2026-10-07
use std::{
    collections::{HashMap, HashSet},
    io,
};
use vivid_sdk::presenter::{
    BridgeSurface, BridgeSurfaceKey, OverlayActionRoute, OverlayInputEpoch,
};

// Match the gateway's bounded external input table and native source budget.
const CAPACITY: usize = 64;
#[derive(Default)]
pub(super) struct InputEpochs {
    next: u64,
    bound: HashMap<BridgeSurfaceKey, (OverlayInputEpoch, u64)>,
}
impl InputEpochs {
    pub(super) fn projection(
        &mut self,
        route: &OverlayActionRoute,
        surfaces: &[BridgeSurface],
    ) -> Vec<(BridgeSurfaceKey, u64)> {
        let desired = surfaces
            .iter()
            .filter(|surface| surface.overlay_window.is_some())
            .map(|s| s.key)
            .collect::<HashSet<_>>();
        self.bound.retain(|key, _| desired.contains(key));
        let mut result = Vec::new();
        for surface in surfaces
            .iter()
            .filter(|surface| surface.overlay_window.is_some())
        {
            let Some(epoch) = route.input_epoch(surface.key) else {
                self.bound.remove(&surface.key);
                continue;
            };
            if let Some((previous, correlation)) = self.bound.get(&surface.key)
                && *previous == epoch
            {
                result.push((surface.key, *correlation));
                continue;
            }
            self.bound.remove(&surface.key);
            if self.bound.len() >= CAPACITY {
                continue;
            }
            let Some(next) = self.next.checked_add(1) else {
                continue;
            };
            self.next = next;
            self.bound.insert(surface.key, (epoch, next));
            result.push((surface.key, next));
        }
        result
    }
    pub(super) fn forward(
        &self,
        route: &OverlayActionRoute,
        correlation: u64,
        surface: BridgeSurfaceKey,
        body: &[u8],
    ) -> io::Result<bool> {
        let Some((epoch, current)) = self.bound.get(&surface) else {
            return Ok(false);
        };
        if correlation == 0 || *current != correlation {
            return Ok(false);
        }
        Ok(route
            .input(*epoch, body)?
            .is_some_and(vivid_sdk::presenter::OverlayRelayInput::forward))
    }
    pub(super) fn reset(&mut self) {
        self.bound.clear();
    }
}
