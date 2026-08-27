//! Bounded multi-track raster/delta producer for W6 public-path saturation and recovery tests.

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
    CoordinateModel, Fit, LaneClass, MILESTONE_OUTPUT_READY, ProducerConfig, RasterDeltaOperation,
    RequestMetadata, SceneNode, SlotBinding, SurfaceDefinition, SurfaceDescriptor, SurfaceRole,
    TrackChannel, TrackWaitCondition,
};

const MAX_TRACKS: usize = 6;

fn main() {
    if let Err(error) = run() {
        eprintln!("W6 public-path fixture failed: {error}");
        std::process::exit(1);
    }
}

fn run() -> io::Result<()> {
    let track_count = bounded("VVMUX_W6_TRACKS", MAX_TRACKS as u64, 1, MAX_TRACKS as u64)? as usize;
    let width = bounded("VVMUX_W6_WIDTH", 128, 1, 1920)? as u32;
    let height = bounded("VVMUX_W6_HEIGHT", 72, 1, 1080)? as u32;
    let frames_per_second = bounded("VVMUX_W6_FPS", 30, 1, 120)?;
    let duration_seconds = bounded("VVMUX_W6_SECONDS", 20, 1, 300)?;
    let inject_bad_delta = env::var_os("VVMUX_W6_BAD_DELTA").is_some();
    let pixel_bytes =
        usize::try_from(media::rgba8_pixel_len(width, height).map_err(io::Error::other)?)
            .map_err(|_| invalid("pixel buffer exceeds address space"))?;
    let maximum_record_body =
        media::rgba8_raw_frame_body_len(width, height).map_err(io::Error::other)?;
    let byte_rate = u64::from(maximum_record_body)
        .checked_mul(frames_per_second)
        .ok_or_else(|| invalid("fixture byte rate overflow"))?;

    let mut client = vivid_sdk::Session::connect(ProducerConfig::default())?;
    let context = client.info().root_context_id;
    let mut channels: Vec<TrackChannel> = Vec::with_capacity(track_count);
    let pixels = vec![0x40_u8; pixel_bytes];
    for index in 0..track_count {
        let surface_id = client.allocate_id()?;
        let track_id = client.allocate_id()?;
        let node_id = client.allocate_id()?;
        let surface = client.create_surface(
            SurfaceDefinition {
                context_id: context,
                surface_id,
                semantic_profile: vivid_sdk::GENERIC_CONTENT.into(),
                coordinate_model: CoordinateModel::DesktopLogicalPixels,
                logical_width: u64::from(width),
                logical_height: u64::from(height),
                scale_numerator: 1,
                scale_denominator: 1,
                rotation: 0,
                descriptor: SurfaceDescriptor {
                    role: SurfaceRole::Figure,
                    title: format!("W6 track {}", index + 1),
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
                    (1, signed(i64::try_from(index).unwrap_or(0) * 8)),
                    (2, signed(i64::try_from(index).unwrap_or(0) * 8)),
                    (3, signed(1_i64 << 32)),
                    (4, signed(1_i64 << 32)),
                    (5, Value::Unsigned(1)),
                ],
                fit: Fit::Contain,
                linear_sampling: true,
                z_index: i64::try_from(index).unwrap_or(i64::MAX),
                visible: true,
                opacity: u16::MAX,
                clip: None,
            },
            &RequestMetadata::default(),
        )?;
        let track = client.create_track(
            TrackConfiguration {
                context_id: context,
                surface_id,
                track_id,
                slot: 3,
                mode: TrackMode::Live,
                lane: LaneClass::Bulk,
                maximum_record_body,
                maximum_rate_millihertz: frames_per_second.saturating_mul(1_000),
                maximum_encoded_bits_per_second: byte_rate.saturating_mul(8),
                maximum_records_per_second: frames_per_second,
                maximum_inflight_body_bytes: u64::from(maximum_record_body).saturating_mul(2),
                kind: KindConfiguration::Raster(RasterConfiguration {
                    width,
                    height,
                    alpha_mode: 1,
                    delta_enabled: true,
                    maximum_delta_operations: 1,
                    zstd_enabled: false,
                }),
                target_latency_us: 100_000,
                maximum_latency_us: 1_000_000,
                retained_pixel_charge: u64::from(width) * u64::from(height),
            },
            &RequestMetadata::default(),
        )?;
        let channel = client.open_track_channel(&track)?;
        channel.send_raster(0, 1, &pixels, false)?;
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
        channels.push(channel);
    }

    println!("W6_FIXTURE_READY tracks={track_count}");
    let interval = Duration::from_nanos(1_000_000_000 / frames_per_second);
    let deadline = Instant::now()
        .checked_add(Duration::from_secs(duration_seconds))
        .ok_or_else(|| invalid("fixture deadline overflow"))?;
    let mut frame_id = 1_u64;
    while Instant::now() < deadline {
        frame_id = frame_id
            .checked_add(1)
            .ok_or_else(|| invalid("fixture frame ID exhausted"))?;
        let color = frame_id as u8;
        let overwrite = [color, color.wrapping_mul(3), color.wrapping_mul(7), 255];
        for (index, channel) in channels.iter().enumerate() {
            let force_full = frame_id == 4 || (inject_bad_delta && index == 0 && frame_id == 3);
            if force_full && frame_id == 4 {
                let mut full = vec![0_u8; pixel_bytes];
                for pixel in full.as_chunks_mut::<4>().0 {
                    pixel.copy_from_slice(&overwrite);
                }
                channel.send_raster(0, frame_id, &full, false)?;
            } else {
                let base = if inject_bad_delta && index == 0 && frame_id == 3 {
                    0
                } else {
                    frame_id - 1
                };
                channel.send_raster_delta(
                    0,
                    frame_id,
                    base,
                    i64::try_from(frame_id.saturating_mul(1_000_000) / frames_per_second)
                        .unwrap_or(i64::MAX),
                    1_000_000 / frames_per_second,
                    &[RasterDeltaOperation::Overwrite {
                        x: 0,
                        y: 0,
                        width: 1,
                        height: 1,
                        rgba: &overwrite,
                    }],
                    false,
                )?;
            }
        }
        thread::sleep(interval.min(deadline.saturating_duration_since(Instant::now())));
    }
    for channel in &channels {
        channel.eos()?;
    }
    client.close()
}

fn bounded(name: &str, default: u64, minimum: u64, maximum: u64) -> io::Result<u64> {
    let value = env::var(name)
        .ok()
        .map(|value| {
            value
                .parse::<u64>()
                .map_err(|_| invalid("fixture setting is not an integer"))
        })
        .transpose()?
        .unwrap_or(default);
    if !(minimum..=maximum).contains(&value) {
        return Err(invalid("fixture setting is outside its bounded range"));
    }
    Ok(value)
}

fn signed(value: i64) -> Value {
    if value >= 0 {
        Value::Unsigned(value as u64)
    } else {
        Value::Negative(value)
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
