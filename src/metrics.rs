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

pub use vivid_gateway::{BridgeMetrics, DeliveryMetrics, IpcMetrics, RelayMetrics};

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
