//! Long-running Vivid 1.5 raster producer for detach/reattach soak testing.

use std::env;
use std::io;
use std::thread;
use std::time::{Duration, Instant};

use vivid_protocol::cbor::Value;
use vivid_protocol::media;
use vivid_protocol::track::{
    KindConfiguration, RasterConfiguration, TrackConfiguration, TrackMode,
};
use vivid_sdk::{
    CoordinateModel, Fit, LaneClass, MILESTONE_OUTPUT_READY, ProducerConfig, RequestMetadata,
    SceneNode, SlotBinding, SurfaceDefinition, SurfaceDescriptor, SurfaceRole, TrackWaitCondition,
};

const DEFAULT_DURATION_SECONDS: u64 = 60 * 60;
const FRAME_INTERVAL: Duration = Duration::from_millis(100);

fn main() {
    if let Err(error) = run() {
        eprintln!("detached media soak failed: {error}");
        std::process::exit(1);
    }
}

fn run() -> io::Result<()> {
    let duration = env::var("VVMUX_MEDIA_SOAK_SECONDS")
        .ok()
        .map(|value| {
            value
                .parse::<u64>()
                .map_err(|_| invalid("VVMUX_MEDIA_SOAK_SECONDS is not an unsigned integer"))
        })
        .transpose()?
        .unwrap_or(DEFAULT_DURATION_SECONDS);
    if duration == 0 || duration > 24 * 60 * 60 {
        return Err(invalid(
            "VVMUX_MEDIA_SOAK_SECONDS must be between 1 and 86400",
        ));
    }

    // ProducerConfig reads VIVID_ENDPOINT_CONTROL and VIVID_ROOT_SECRET. Realtime and bulk
    // discovery intentionally fall back to the control endpoint in a vvmux pane.
    let mut client = vivid_sdk::Session::connect(ProducerConfig::default())?;
    let context = client.info().root_context_id;
    let surface_id = client.allocate_id()?;
    let track_id = client.allocate_id()?;
    let node_id = client.allocate_id()?;
    let surface = client.create_surface(
        SurfaceDefinition {
            context_id: context,
            surface_id,
            semantic_profile: vivid_sdk::GENERIC_CONTENT.into(),
            coordinate_model: CoordinateModel::DesktopLogicalPixels,
            logical_width: 1,
            logical_height: 1,
            scale_numerator: 1,
            scale_denominator: 1,
            rotation: 0,
            descriptor: SurfaceDescriptor {
                role: SurfaceRole::Figure,
                title: "vvmux detached raster soak".into(),
                semantic_content_revision: 1,
                semantic_availability: 0,
                locator_hint: String::new(),
            },
            policy: 0,
            profile_parameters: vec![],
        },
        &RequestMetadata::default(),
    )?;
    client.create_node(
        &SceneNode {
            owning_context_id: context,
            node_id,
            surface_context_id: context,
            surface_id,
            geometry: vec![
                (0, Value::Unsigned(1)),
                (1, signed(0)),
                (2, signed(0)),
                (3, signed(1_i64 << 32)),
                (4, signed(1_i64 << 32)),
                (5, Value::Unsigned(1)),
            ],
            fit: Fit::Contain,
            linear_sampling: true,
            z_index: 0,
            visible: true,
            opacity: u16::MAX,
            clip: None,
        },
        &RequestMetadata::default(),
    )?;
    let maximum_record_body = media::rgba8_raw_frame_body_len(1, 1).map_err(io::Error::other)?;
    let track = client.create_track(
        TrackConfiguration {
            context_id: context,
            surface_id,
            track_id,
            slot: 3,
            mode: TrackMode::Live,
            lane: LaneClass::Bulk,
            maximum_record_body,
            maximum_rate_millihertz: 10_000,
            maximum_encoded_bits_per_second: u64::from(maximum_record_body) * 8 * 10,
            maximum_records_per_second: 10,
            maximum_inflight_body_bytes: u64::from(maximum_record_body) * 2,
            kind: KindConfiguration::Raster(RasterConfiguration {
                width: 1,
                height: 1,
                alpha_mode: 1,
                delta_enabled: false,
                maximum_delta_operations: 1,
                zstd_enabled: false,
            }),
            target_latency_us: 100_000,
            maximum_latency_us: 1_000_000,
            retained_pixel_charge: 1,
        },
        &RequestMetadata::default(),
    )?;
    let channel = client.open_track_channel(&track)?;

    channel.send_raster(0, 1, &[0, 0, 0, 255], false)?;
    client.wait_track(
        &track,
        TrackWaitCondition::MilestoneSet,
        Some(MILESTONE_OUTPUT_READY),
        5_000_000,
    )?;
    client.activate_tracks(
        &surface,
        &[SlotBinding {
            slot: 3,
            track_id,
            expected_channel_generation: track.channel_generation(),
            required_milestone: MILESTONE_OUTPUT_READY,
        }],
        &RequestMetadata::default(),
    )?;

    let deadline = Instant::now()
        .checked_add(Duration::from_secs(duration))
        .ok_or_else(|| invalid("media soak deadline overflowed"))?;
    let mut frame_id = 1_u64;
    while Instant::now() < deadline {
        frame_id = frame_id
            .checked_add(1)
            .ok_or_else(|| invalid("raster frame ID exhausted"))?;
        let phase = frame_id as u8;
        channel.send_raster(
            0,
            frame_id,
            &[phase, phase.wrapping_mul(3), phase.wrapping_mul(7), 255],
            false,
        )?;
        thread::sleep(FRAME_INTERVAL.min(deadline.saturating_duration_since(Instant::now())));
    }
    channel.eos()?;
    client.close()
}

fn signed(value: i64) -> Value {
    if value >= 0 {
        Value::Unsigned(value as u64)
    } else {
        Value::Negative(value)
    }
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}
