//! Owner-only relay counters.
//!
//! These exist to make the relay's cost measurable before and after a throughput change: without
//! them a claim like "the client hop costs 3.5x its payload" is unverifiable. They are diagnostic,
//! never a control input, and nothing in the media, projection, or flow-control paths reads them.
//!
//! Counters are monotonic and saturating. A relaxed atomic is deliberate: a torn read across two
//! counters is acceptable for diagnostics and the media path must not pay for ordering it does not
//! need.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Byte and record totals for one VVMX connection, plus the time its writer spent blocked.
///
/// `wire_*` counts framed bytes including the 16-byte VVMX header. `*_payload_bytes` counts the
/// caller's bytes before framing, so `wire_bytes_written / payload_bytes` is the encoding
/// amplification of the hop.
#[derive(Debug, Default)]
pub struct IpcCounters {
    pub records_written: AtomicU64,
    pub wire_bytes_written: AtomicU64,
    pub records_read: AtomicU64,
    pub wire_bytes_read: AtomicU64,
    pub media_payload_bytes: AtomicU64,
    pub media_records: AtomicU64,
    pub render_payload_bytes: AtomicU64,
    pub write_blocked_us: AtomicU64,
}

impl IpcCounters {
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn record_write(&self, wire_bytes: usize, blocked: Duration) {
        self.records_written.fetch_add(1, Ordering::Relaxed);
        self.wire_bytes_written
            .fetch_add(wire_bytes as u64, Ordering::Relaxed);
        self.write_blocked_us.fetch_add(
            blocked.as_micros().min(u128::from(u64::MAX)) as u64,
            Ordering::Relaxed,
        );
    }

    pub fn record_read(&self, wire_bytes: usize) {
        self.records_read.fetch_add(1, Ordering::Relaxed);
        self.wire_bytes_read
            .fetch_add(wire_bytes as u64, Ordering::Relaxed);
    }

    pub fn record_media_payload(&self, bytes: usize) {
        self.media_records.fetch_add(1, Ordering::Relaxed);
        self.media_payload_bytes
            .fetch_add(bytes as u64, Ordering::Relaxed);
    }

    pub fn record_render_payload(&self, bytes: usize) {
        self.render_payload_bytes
            .fetch_add(bytes as u64, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> IpcMetrics {
        IpcMetrics {
            records_written: self.records_written.load(Ordering::Relaxed),
            wire_bytes_written: self.wire_bytes_written.load(Ordering::Relaxed),
            records_read: self.records_read.load(Ordering::Relaxed),
            wire_bytes_read: self.wire_bytes_read.load(Ordering::Relaxed),
            media_payload_bytes: self.media_payload_bytes.load(Ordering::Relaxed),
            media_records: self.media_records.load(Ordering::Relaxed),
            render_payload_bytes: self.render_payload_bytes.load(Ordering::Relaxed),
            write_blocked_us: self.write_blocked_us.load(Ordering::Relaxed),
        }
    }
}

/// Times a blocking section without allocating or syscalling when the section is fast.
pub struct BlockTimer(Instant);

impl BlockTimer {
    pub fn start() -> Self {
        Self(Instant::now())
    }

    pub fn elapsed(self) -> Duration {
        self.0.elapsed()
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct IpcMetrics {
    pub records_written: u64,
    pub wire_bytes_written: u64,
    pub records_read: u64,
    pub wire_bytes_read: u64,
    pub media_payload_bytes: u64,
    pub media_records: u64,
    pub render_payload_bytes: u64,
    pub write_blocked_us: u64,
}

/// Virtual-presenter media accounting, owned by the session server.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeliveryMetrics {
    /// Deliveries handed to the bridge.
    pub created: u64,
    /// Deliveries the bridge reported as written to the outer presenter.
    pub delivered: u64,
    /// Deliveries the bridge reported as not written.
    pub failed: u64,
    /// Records that never became a delivery because the actor event channel was full.
    pub dropped_actor_queue_full: u64,
    /// Records that never became a delivery because the queued-byte budget was exhausted.
    pub dropped_queue_budget: u64,
    /// Deliveries released because their source stopped being projected.
    pub released_hidden: u64,
    /// `NEED_KEYFRAME` records written to inner producers.
    pub keyframe_requests: u64,
    /// `NEED_KEYFRAME` records suppressed because recovery was already outstanding.
    pub keyframe_requests_damped: u64,
}

/// Foreground-bridge accounting, reported back to the session server for diagnostics.
///
/// The bridge runs in the client process, so these counters travel over VVMX rather than being
/// read directly. They are advisory and may lag by one report interval.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct BridgeMetrics {
    /// Media bodies written toward the outer presenter.
    pub outer_media_records: u64,
    /// Payload bytes written toward the outer presenter.
    pub outer_media_bytes: u64,
    /// Raster bodies re-originated as full frames.
    pub outer_raster_full_frames: u64,
    /// Raster bodies re-originated as deltas.
    pub outer_raster_delta_frames: u64,
    /// Payload bytes of raster bodies before re-origination.
    pub inner_raster_bytes: u64,
    /// Records dropped because the client media queue was full.
    pub client_queue_drops: u64,
    /// Time the bridge worker spent waiting for an outer control reply.
    pub control_wait_us: u64,
    /// Outer control waits that hit their deadline.
    pub control_wait_timeouts: u64,
    /// Outer session replacements.
    pub session_replacements: u64,
}

/// Everything `vvmux msg inspect-media` reports about the relay itself.
///
/// This is session-scoped rather than pane-scoped: it describes the single client connection and
/// the single foreground bridge shared by every pane.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RelayMetrics {
    pub ipc: IpcMetrics,
    pub delivery: DeliveryMetrics,
    pub bridge: BridgeMetrics,
}
