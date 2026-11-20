//! Native document events between the foreground bridge and its same-host session server.

// Rust guideline compliant 2026-10-07
use serde::{Deserialize, Serialize};
use std::{fmt, io, time::Instant};
use vivid_gateway::ExternalDocumentInput;
use vivid_protocol::{cbor::Value, messages, overlay::wire::text::editor::Edit};
use vivid_sdk::presenter::BridgeSurfaceKey;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DocumentInput {
    pub bridge: u64,
    pub sequence: u64,
    pub publication: u64,
    pub surface: BridgeSurfaceKey,
    deadline_us: u64,
    body: Vec<u8>,
}

impl fmt::Debug for DocumentInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DocumentInput")
            .field("surface", &self.surface)
            .field("publication", &self.publication)
            .finish_non_exhaustive()
    }
}

impl DocumentInput {
    pub fn encode(
        input: ExternalDocumentInput,
        bridge: u64,
        sequence: u64,
    ) -> io::Result<Option<Self>> {
        // Sample the shared clock first, so time spent sampling can only shorten the lifetime.
        let now = monotonic_us()?;
        let Some(remaining) = input.deadline.checked_duration_since(Instant::now()) else {
            return Ok(None);
        };
        let remaining = u64::try_from(remaining.as_micros()).map_err(io::Error::other)?;
        let deadline_us = now
            .checked_add(remaining)
            .ok_or_else(|| io::Error::other("document deadline overflow"))?;
        Ok(Some(Self {
            bridge,
            sequence,
            publication: input.publication,
            surface: input.surface,
            deadline_us,
            body: messages::Envelope::new(0, input.edit.payload()?).encode()?,
        }))
    }

    pub fn decode(&self) -> io::Result<Option<(Edit, Instant)>> {
        let started = Instant::now();
        let now = monotonic_us()?;
        if self.deadline_us <= now || self.deadline_us - now > 1_000_000 {
            return Ok(None);
        }
        let envelope = messages::decode_control(&self.body)?;
        if envelope.request_id != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unexpected document request ID",
            ));
        }
        let edit = Edit::decode(self.surface.surface, &Value::Map(envelope.payload))?;
        Ok(Some((
            edit,
            started + std::time::Duration::from_micros(self.deadline_us - now),
        )))
    }
}

// Native clients and the hidden server share this clock origin. Network gateways
// do not opt into document editing.
#[cfg(unix)]
fn monotonic_us() -> io::Result<u64> {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: ts is a writable timespec and CLOCK_MONOTONIC has no per-process origin.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &raw mut ts) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let secs = u64::try_from(ts.tv_sec).map_err(io::Error::other)?;
    let nanos = u64::try_from(ts.tv_nsec).map_err(io::Error::other)?;
    secs.checked_mul(1_000_000)
        .and_then(|us| us.checked_add(nanos / 1000))
        .ok_or_else(|| io::Error::other("monotonic clock overflow"))
}

#[cfg(windows)]
fn monotonic_us() -> io::Result<u64> {
    use windows_sys::Win32::System::Performance::{
        QueryPerformanceCounter, QueryPerformanceFrequency,
    };
    let (mut ticks, mut frequency) = (0, 0);
    // SAFETY: both calls write to valid exclusive i64 storage.
    if unsafe { QueryPerformanceFrequency(&raw mut frequency) } == 0
        || unsafe { QueryPerformanceCounter(&raw mut ticks) } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let ticks = u128::try_from(ticks).map_err(io::Error::other)?;
    let frequency = u128::try_from(frequency).map_err(io::Error::other)?;
    let micros = (ticks * 1_000_000)
        .checked_div(frequency)
        .ok_or_else(|| io::Error::other("invalid performance frequency"))?;
    u64::try_from(micros).map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use vivid_protocol::overlay::wire::WindowAddress;

    #[test]
    fn queued_ipc_bytes_cannot_refresh_the_physical_document_deadline() {
        let input = |deadline| ExternalDocumentInput {
            publication: 9,
            surface: BridgeSurfaceKey {
                producer: 3,
                context: 1,
                surface: 1,
            },
            edit: Edit {
                address: WindowAddress {
                    context_id: 1,
                    surface_id: 1,
                    generation: 1,
                },
                scene_revision: 2,
                editor: 41,
                revision: 37,
                replacement: Some(1..7),
                selection: None,
                commit: Some("好".into()),
                preedit: None,
            },
            deadline,
        };
        let end = Instant::now() + Duration::from_millis(40);
        let message = DocumentInput::encode(input(end), 5, 1).unwrap().unwrap();
        let bytes = serde_json::to_vec(&message).unwrap();
        std::thread::sleep(end.saturating_duration_since(Instant::now()));
        let delayed: DocumentInput = serde_json::from_slice(&bytes).unwrap();
        assert!(delayed.decode().unwrap().is_none());
        let end = Instant::now() + Duration::from_secs(1);
        let fresh = DocumentInput::encode(input(end), 5, 2).unwrap().unwrap();
        let (edit, remaining) = fresh.decode().unwrap().unwrap();
        assert_eq!(edit.commit.as_deref(), Some("好"));
        assert!(remaining <= end + Duration::from_micros(1));
    }
}
