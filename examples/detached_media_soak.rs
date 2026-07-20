use std::env;
use std::io;
use std::thread;
use std::time::{Duration, Instant};

use vivid_protocol::media;
use vivid_protocol::messages;
use vivid_protocol::wire::{Connection, ConnectionKind, Endpoint, Record};
use zeroize::Zeroizing;

const SOURCE_ID: u64 = 1;
const DEFAULT_DURATION_SECONDS: u64 = 60 * 60;
const FRAME_INTERVAL: Duration = Duration::from_millis(100);

fn main() {
    if let Err(error) = run() {
        eprintln!("detached media soak failed: {error}");
        std::process::exit(1);
    }
}

fn run() -> io::Result<()> {
    let endpoint = env::var("VIVID_ENDPOINT")
        .map_err(|_| invalid("VIVID_ENDPOINT is not present in the pane environment"))?;
    let token = Zeroizing::new(
        env::var("VIVID_TOKEN")
            .map_err(|_| invalid("VIVID_TOKEN is not present in the pane environment"))?,
    );
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

    let endpoint = Endpoint::parse(&endpoint)?;
    let mut control = Connection::open(&endpoint, ConnectionKind::Control)?;
    control.write_record(messages::HELLO, 0, 0, &messages::hello(1, &token))?;
    let welcome = read_until(&mut control, messages::WELCOME, 0)?;
    let welcome = messages::parse_welcome(&welcome.body)?;
    control.set_send_body_limit(welcome.maximum_control_body)?;

    control.write_record(
        messages::CREATE_RASTER,
        0,
        SOURCE_ID,
        &messages::create_raster(2, SOURCE_ID, 1, 1),
    )?;
    let source_ready = read_until(&mut control, messages::SOURCE_READY, SOURCE_ID)?;
    let source_ready = messages::parse_source_ready(&source_ready.body)?;

    let mut raster = Connection::open(&endpoint, ConnectionKind::Raster)?;
    raster.write_record(
        messages::ATTACH_CHANNEL,
        0,
        SOURCE_ID,
        &messages::attach_channel(&source_ready.media_ticket),
    )?;

    let started = Instant::now();
    let deadline = started
        .checked_add(Duration::from_secs(duration))
        .ok_or_else(|| invalid("media soak deadline overflowed"))?;
    let mut frame_id = 0_u64;
    while Instant::now() < deadline {
        frame_id = frame_id
            .checked_add(1)
            .ok_or_else(|| invalid("media soak frame ID exhausted"))?;
        let phase = frame_id as u8;
        let pixels = [phase, phase.wrapping_mul(3), phase.wrapping_mul(7), 255];
        let body = media::raster_frame_body(0, frame_id, 1, 1, &pixels)?;
        raster.write_record(messages::RASTER_FRAME, 0, SOURCE_ID, &body)?;

        let credit = read_until(&mut control, messages::CREDIT, SOURCE_ID)?;
        let credit = messages::parse_credit(&credit.body)?;
        if credit.packets != 1 || credit.bytes != body.len() as u64 {
            return Err(invalid("presenter returned an invalid raster credit"));
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        thread::sleep(FRAME_INTERVAL.min(remaining));
    }

    control.write_record(messages::GOODBYE, 0, 0, &messages::goodbye(3))?;
    let goodbye = read_until(&mut control, messages::OK, 0)?;
    if messages::request_id(&goodbye.body)? != 3 {
        return Err(invalid("presenter returned an uncorrelated GOODBYE reply"));
    }
    Ok(())
}

fn read_until(
    connection: &mut Connection,
    expected_type: u16,
    expected_object: u64,
) -> io::Result<Record> {
    loop {
        let record = connection.read_record()?;
        if record.record_type == messages::PING {
            let envelope = messages::decode_control(&record.body)?;
            if record.object_id != 0 || envelope.request_id == 0 {
                return Err(invalid("presenter sent an invalid PING"));
            }
            connection.write_record(messages::PONG, 0, 0, &messages::ok(envelope.request_id))?;
            continue;
        }
        if record.record_type == messages::ERROR {
            return Err(invalid(messages::parse_error(&record.body)?));
        }
        if record.record_type == expected_type && record.object_id == expected_object {
            return Ok(record);
        }
        if record.flags & vivid_protocol::wire::RECORD_OPTIONAL != 0 {
            continue;
        }
        return Err(invalid(format!(
            "unexpected Vivid record type {:#06x} for object {}",
            record.record_type, record.object_id
        )));
    }
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}
