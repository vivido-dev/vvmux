use std::collections::VecDeque;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::ipc::{BridgePlayRequest, BridgeSourceKey};

pub const MAX_MEDIA_TRACE_EVENTS: usize = 4096;
pub const MAX_MEDIA_TRACE_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_MEDIA_TRACE_QUERY_EVENTS: u16 = 512;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
#[clap(rename_all = "kebab-case")]
pub enum MediaTraceCategory {
    Bridge,
    Projection,
    Playback,
    Recovery,
    Delivery,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MediaPlaybackControl {
    Play,
    Pause,
    Eos,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MediaKeyframeStage {
    OuterRequested,
    ProducerQueued,
    ProducerWritten,
    ProducerForwarded,
    ProducerDamped,
    ProducerIgnored,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum MediaTraceKind {
    BridgeClientAttached {
        vivid: bool,
    },
    BridgeClientDetached,
    ProjectionSubmitted {
        virtual_revision: u64,
        surface_count: u16,
        track_count: u16,
        node_count: u16,
    },
    ProjectionApplied {
        virtual_revision: u64,
        bridge_local_revision: u64,
        attachment_count: u16,
    },
    TrackVisibility {
        visible: bool,
        virtual_revision: u64,
    },
    OuterTrackRecreated {
        attachment_generation: u64,
        playing: bool,
    },
    OuterTrackRemoved,
    PlaybackControl {
        control: MediaPlaybackControl,
        request: Option<BridgePlayRequest>,
    },
    PlaybackState {
        state: u64,
        eos_state: u64,
    },
    KeyframeRequest {
        stage: MediaKeyframeStage,
        minimum_epoch: Option<u32>,
        reason: u64,
    },
    KeyframeRecovered {
        epoch: u32,
        pts_us: i64,
    },
    KeyframeDelivery {
        delivery_id: u64,
        delivered: bool,
        epoch: u32,
        pts_us: i64,
    },
    SnapshotRetry,
    TrackLost,
    DeliveryFailed {
        delivery_id: u64,
    },
    QueueDrops {
        dropped: u64,
        total: u64,
    },
}

impl MediaTraceKind {
    pub fn category(&self) -> MediaTraceCategory {
        match self {
            Self::BridgeClientAttached { .. } | Self::BridgeClientDetached => {
                MediaTraceCategory::Bridge
            }
            Self::ProjectionSubmitted { .. }
            | Self::ProjectionApplied { .. }
            | Self::TrackVisibility { .. } => MediaTraceCategory::Projection,
            Self::PlaybackControl { .. } | Self::PlaybackState { .. } => {
                MediaTraceCategory::Playback
            }
            Self::OuterTrackRecreated { .. }
            | Self::KeyframeRequest { .. }
            | Self::KeyframeRecovered { .. }
            | Self::KeyframeDelivery { .. }
            | Self::SnapshotRetry
            | Self::TrackLost => MediaTraceCategory::Recovery,
            Self::OuterTrackRemoved | Self::DeliveryFailed { .. } | Self::QueueDrops { .. } => {
                MediaTraceCategory::Delivery
            }
        }
    }

    pub fn is_recovery(&self) -> bool {
        matches!(
            self,
            Self::OuterTrackRecreated { .. }
                | Self::KeyframeRequest { .. }
                | Self::KeyframeRecovered { .. }
                | Self::KeyframeDelivery { .. }
                | Self::SnapshotRetry
                | Self::TrackLost
                | Self::DeliveryFailed { .. }
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BridgeMediaTraceEvent {
    pub origin_monotonic_us: u64,
    #[serde(rename = "track")]
    pub source: Option<BridgeSourceKey>,
    pub kind: MediaTraceKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MediaTraceEvent {
    pub sequence: u64,
    pub process_id: u32,
    pub process_instance_id: String,
    pub startup_wall_clock_unix_ms: u64,
    pub monotonic_us: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin_monotonic_us: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "virtual_track")]
    pub virtual_source: Option<BridgeSourceKey>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bridge_instance_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovery_sequence: Option<u64>,
    pub kind: MediaTraceKind,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct MediaTraceFilter {
    pub producer_id: Option<u64>,
    pub context_id: Option<u64>,
    pub surface_id: Option<u64>,
    pub track_id: Option<u64>,
    pub category: Option<MediaTraceCategory>,
    pub recovery_only: bool,
}

impl MediaTraceFilter {
    fn matches(&self, event: &MediaTraceEvent) -> bool {
        if self.recovery_only && !event.kind.is_recovery() {
            return false;
        }
        if self
            .category
            .is_some_and(|category| event.kind.category() != category)
        {
            return false;
        }
        if let Some(producer) = self.producer_id
            && event.virtual_source.map(|source| source.producer) != Some(producer)
        {
            return false;
        }
        if let Some(context_id) = self.context_id
            && event.virtual_source.map(|source| source.context) != Some(context_id)
        {
            return false;
        }
        if let Some(surface_id) = self.surface_id
            && event.virtual_source.map(|source| source.surface) != Some(surface_id)
        {
            return false;
        }
        if let Some(track_id) = self.track_id
            && event.virtual_source.map(|source| source.track) != Some(track_id)
        {
            return false;
        }
        true
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MediaTraceGap {
    pub requested_sequence: u64,
    pub oldest_sequence: u64,
    pub current_sequence: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MediaTraceBatch {
    pub oldest_sequence: u64,
    pub current_sequence: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gap: Option<MediaTraceGap>,
    pub events: Vec<MediaTraceEvent>,
}

struct StoredEvent {
    bytes: usize,
    event: MediaTraceEvent,
}

pub struct MediaTraceJournal {
    started: Instant,
    startup_wall_clock_unix_ms: u64,
    next_sequence: u64,
    bytes: usize,
    events: VecDeque<StoredEvent>,
    maximum_events: usize,
    maximum_bytes: usize,
}

impl Default for MediaTraceJournal {
    fn default() -> Self {
        Self::with_limits(MAX_MEDIA_TRACE_EVENTS, MAX_MEDIA_TRACE_BYTES)
    }
}

impl MediaTraceJournal {
    fn with_limits(maximum_events: usize, maximum_bytes: usize) -> Self {
        Self {
            started: Instant::now(),
            startup_wall_clock_unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |duration| {
                    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
                }),
            next_sequence: 0,
            bytes: 0,
            events: VecDeque::new(),
            maximum_events,
            maximum_bytes,
        }
    }

    pub fn push(
        &mut self,
        process_instance_id: &str,
        pane_id: Option<u64>,
        virtual_source: Option<BridgeSourceKey>,
        bridge_instance_id: Option<u64>,
        origin_monotonic_us: Option<u64>,
        kind: MediaTraceKind,
    ) {
        self.next_sequence = self.next_sequence.saturating_add(1);
        let recovery_sequence = kind.is_recovery().then_some(self.next_sequence);
        let event = MediaTraceEvent {
            sequence: self.next_sequence,
            process_id: std::process::id(),
            process_instance_id: process_instance_id.to_owned(),
            startup_wall_clock_unix_ms: self.startup_wall_clock_unix_ms,
            monotonic_us: u64::try_from(self.started.elapsed().as_micros()).unwrap_or(u64::MAX),
            origin_monotonic_us,
            pane_id,
            virtual_source,
            bridge_instance_id,
            recovery_sequence,
            kind,
        };
        let bytes = serde_json::to_vec(&event).map_or(0, |encoded| encoded.len());
        if bytes > self.maximum_bytes {
            return;
        }
        self.bytes = self.bytes.saturating_add(bytes);
        self.events.push_back(StoredEvent { bytes, event });
        while self.events.len() > self.maximum_events || self.bytes > self.maximum_bytes {
            if let Some(removed) = self.events.pop_front() {
                self.bytes = self.bytes.saturating_sub(removed.bytes);
            } else {
                break;
            }
        }
    }

    pub fn query(
        &self,
        after_sequence: Option<u64>,
        limit: u16,
        pane_id: Option<u64>,
        filter: MediaTraceFilter,
    ) -> MediaTraceBatch {
        let oldest_sequence = self
            .events
            .front()
            .map_or(self.next_sequence.saturating_add(1), |stored| {
                stored.event.sequence
            });
        let requested = after_sequence.unwrap_or(oldest_sequence.saturating_sub(1));
        let gap = (after_sequence.is_some() && requested < oldest_sequence.saturating_sub(1))
            .then_some(MediaTraceGap {
                requested_sequence: requested,
                oldest_sequence,
                current_sequence: self.next_sequence,
            });
        let events = self
            .events
            .iter()
            .filter(|stored| stored.event.sequence > requested)
            .filter(|stored| {
                pane_id.is_none_or(|pane| {
                    stored.event.pane_id.is_none() || stored.event.pane_id == Some(pane)
                })
            })
            .filter(|stored| filter.matches(&stored.event))
            .take(usize::from(limit.min(MAX_MEDIA_TRACE_QUERY_EVENTS)))
            .map(|stored| stored.event.clone())
            .collect();
        MediaTraceBatch {
            oldest_sequence,
            current_sequence: self.next_sequence,
            gap,
            events,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(id: u64) -> BridgeSourceKey {
        BridgeSourceKey {
            producer: 7,
            context: 1,
            surface: 2,
            track: id,
        }
    }

    #[test]
    fn journal_reports_eviction_gaps_and_keeps_sequences_monotonic() {
        let mut journal = MediaTraceJournal::with_limits(2, 64 * 1024);
        for id in 1..=3 {
            journal.push(
                "test-instance",
                Some(id),
                Some(source(id)),
                Some(9),
                None,
                MediaTraceKind::TrackVisibility {
                    visible: true,
                    virtual_revision: id,
                },
            );
        }

        let batch = journal.query(Some(0), 32, None, MediaTraceFilter::default());
        assert_eq!(batch.current_sequence, 3);
        assert_eq!(
            batch
                .events
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            [2, 3]
        );
        assert_eq!(
            batch.gap,
            Some(MediaTraceGap {
                requested_sequence: 0,
                oldest_sequence: 2,
                current_sequence: 3,
            })
        );
    }

    #[test]
    fn pane_source_category_and_recovery_filters_are_exact() {
        let mut journal = MediaTraceJournal::default();
        journal.push(
            "test-instance",
            Some(2),
            Some(source(1)),
            Some(4),
            None,
            MediaTraceKind::KeyframeRequest {
                stage: MediaKeyframeStage::ProducerQueued,
                minimum_epoch: Some(1),
                reason: 1,
            },
        );
        journal.push(
            "test-instance",
            Some(3),
            Some(source(2)),
            Some(4),
            None,
            MediaTraceKind::PlaybackState {
                state: 2,
                eos_state: 0,
            },
        );

        let batch = journal.query(
            None,
            32,
            Some(2),
            MediaTraceFilter {
                producer_id: Some(7),
                context_id: Some(1),
                surface_id: Some(2),
                track_id: Some(1),
                category: Some(MediaTraceCategory::Recovery),
                recovery_only: true,
            },
        );
        assert_eq!(batch.events.len(), 1);
        assert!(matches!(
            batch.events[0].kind,
            MediaTraceKind::KeyframeRequest { .. }
        ));
    }
}
