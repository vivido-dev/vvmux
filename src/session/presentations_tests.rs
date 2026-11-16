//! Original receipt validation over two authenticated producer sockets.

// Rust guideline compliant 2026-10-07
use super::*;
use std::time::{Duration, Instant};
use vivid_sdk::overlay::{Canvas, OverlaySession, OverlayWindowOptions, Rect, WindowMode};
use vivid_sdk::{ProducerAuthentication, ProducerConfig};

fn connect(vivid: &VirtualVivid, endpoint: &str, pane: u64) -> OverlaySession {
    vivid.update_metrics(pane, 100, 40, (8, 16));
    let secret = vivid.issue_pane_capability(pane).unwrap();
    OverlaySession::connect(ProducerConfig {
        authentication: ProducerAuthentication::root_hex(&secret).unwrap(),
        endpoint_control: Some(endpoint.to_owned()),
        endpoint_interactive: Some(endpoint.to_owned()),
        endpoint_bulk: Some(endpoint.to_owned()),
        endpoint_realtime: Some(endpoint.to_owned()),
        ..ProducerConfig::default()
    })
    .unwrap()
}
fn frames(vivid: &VirtualVivid, count: usize, revision: u64) -> Vec<(BridgeSourceKey, Arc<[u8]>)> {
    let until = Instant::now() + Duration::from_secs(3);
    loop {
        let snapshot = vivid.projection_snapshot(&HashSet::from([1, 2]));
        let frames = snapshot
            .sources
            .iter()
            .flat_map(|source| {
                source
                    .retained_vector
                    .iter()
                    .filter(|(kind, body)| {
                        *kind == vivid_protocol::messages::VECTOR_FRAME
                            && vivid_protocol::vector::Frame::decode(body)
                                .is_ok_and(|frame| frame.revision == revision)
                    })
                    .map(|(_, body)| (source.key, Arc::clone(body)))
            })
            .collect::<Vec<_>>();
        if frames.len() == count {
            return frames;
        }
        assert!(Instant::now() < until, "original source frame stalled");
        std::thread::sleep(Duration::from_millis(1));
    }
}
fn retain(
    manager: &mut NativePresentations,
    correlation: u64,
    source: BridgeSourceKey,
    revision: u64,
    bridge: Option<u64>,
) {
    let attempt = manager
        .route
        .begin_capture(source, 1, revision)
        .unwrap()
        .unwrap();
    let attempt = Arc::new(attempt);
    let OverlayPresentationCapture::Forwarded(receipt) = attempt.capture().unwrap() else {
        panic!("missing original grant")
    };
    manager.copies.insert(
        correlation,
        Copy {
            client: 7,
            bridge,
            source,
            projection: 1,
            receipt,
            attempt: Arc::clone(&attempt),
        },
    );
    manager.seen.insert(
        source,
        Seen {
            attempt,
            projection: 1,
        },
    );
}
#[test]
fn outcomes_require_exact_client_bridge_source_and_original_copy() {
    let vivid = VirtualVivid::start_configured_eventless(
        vivid_sdk::presenter::SocketListener::bind("tcp:127.0.0.1:0").unwrap(),
        vivid_sdk::presenter::PresenterConfig::terminal_with_overlay(
            vivid_sdk::presenter::MediaConfig::default(),
        ),
    )
    .unwrap();
    vivid.enable_overlay_host_relay();
    let endpoint = vivid.endpoint();
    let mut manager =
        NativePresentations::new(vivid.attach_overlay_presentation_relay().unwrap(), 1024);
    let first = connect(&vivid, &endpoint, 1);
    let second = connect(&vivid, &endpoint, 2);
    let a = first
        .create_window(OverlayWindowOptions::new(
            Rect::new(0., 0., 32., 32.).unwrap(),
            WindowMode::Floating,
        ))
        .unwrap();
    let b = second
        .create_window(OverlayWindowOptions::new(
            Rect::new(40., 0., 32., 32.).unwrap(),
            WindowMode::Floating,
        ))
        .unwrap();
    let one = a.submit(Canvas::new()).unwrap();
    let peer = b.submit(Canvas::new()).unwrap();
    let initial = frames(&vivid, 2, 1);
    let x = initial[0].0;
    let y = initial[1].0;
    assert_ne!(x.producer, y.producer);
    assert_eq!(
        (x.context, x.surface, x.track),
        (y.context, y.surface, y.track)
    );
    retain(&mut manager, 1, x, 1, None);
    retain(&mut manager, 2, y, 1, Some(10));
    manager.report(7, 10, 1, x, Some(0)); // Not yet bound to an authenticated bridge.
    manager.bind(7, 10);
    for (client, bridge, correlation, source, outcome) in [
        (8, 10, 1, x, Some(0)),
        (7, 11, 1, x, Some(0)),
        (7, 10, 99, x, Some(0)),
        (7, 10, 1, y, Some(0)),
        (7, 10, 1, x, Some(2)),
    ] {
        manager.report(client, bridge, correlation, source, outcome);
    }
    assert_eq!(manager.copies.len(), 2);
    assert_eq!(one.wait(Duration::ZERO).unwrap(), None);
    assert_eq!(peer.wait(Duration::ZERO).unwrap(), None);
    // Retire one actual owner, leaving the same numeric IDs on the other.
    first.close().unwrap();
    let remaining = frames(&vivid, 1, 1);
    let survivor = remaining[0].0;
    manager.reconcile(&vivid, &HashSet::from([survivor]));
    assert_eq!(manager.copies.len(), 1);
    let correlation = if survivor == x { 1 } else { 2 };
    manager.report(7, 10, correlation, survivor, Some(0));
    assert_eq!(
        peer.wait(Duration::from_secs(3)).unwrap(),
        Some(PresentationOutcome::Presented)
    );
    // A duplicate outcome cannot complete a subsequent scene.
    b.set_bounds(Rect::new(48., 0., 32., 32.).unwrap()).unwrap();
    let next = b.submit(Canvas::new()).unwrap();
    frames(&vivid, 1, 2);
    retain(&mut manager, 3, survivor, 2, Some(10));
    manager.report(7, 10, correlation, survivor, Some(0));
    assert_eq!(next.wait(Duration::ZERO).unwrap(), None);
    manager.reset(&vivid);
    manager.report(7, 10, 3, survivor, Some(0));
    assert_eq!(next.wait(Duration::ZERO).unwrap(), None);
    retain(&mut manager, 4, survivor, 2, Some(11));
    manager.report(7, 10, 4, survivor, Some(0));
    assert_eq!(next.wait(Duration::ZERO).unwrap(), None);
    manager.report(7, 11, 4, survivor, Some(0));
    assert_eq!(
        next.wait(Duration::from_secs(3)).unwrap(),
        Some(PresentationOutcome::Presented)
    );
    let snapshot = vivid.projection_snapshot(&HashSet::from([2]));
    assert_eq!(snapshot.sources.len(), 1);
    assert!(
        snapshot
            .bridge_projection()
            .surfaces
            .iter()
            .any(|surface| surface.overlay_window.is_some_and(|window| window.x == 48))
    );
    second.close().unwrap();
    vivid.shutdown();
}
#[test]
fn pending_storage_coalesces_exact_origins_without_charging_original_copy_credit() {
    let vivid = VirtualVivid::start_configured_eventless(
        vivid_sdk::presenter::SocketListener::bind("tcp:127.0.0.1:0").unwrap(),
        vivid_sdk::presenter::PresenterConfig::terminal_with_overlay(
            vivid_sdk::presenter::MediaConfig::default(),
        ),
    )
    .unwrap();
    vivid.enable_overlay_host_relay();
    let endpoint = vivid.endpoint();
    let mut manager =
        NativePresentations::new(vivid.attach_overlay_presentation_relay().unwrap(), 1024);
    let a = connect(&vivid, &endpoint, 1);
    let b = connect(&vivid, &endpoint, 2);
    let windows = [&a, &b].map(|session| {
        session
            .create_window(OverlayWindowOptions::new(
                Rect::new(0., 0., 32., 32.).unwrap(),
                WindowMode::Floating,
            ))
            .unwrap()
    });
    for window in &windows {
        window.submit(Canvas::new()).unwrap();
    }
    let initial = frames(&vivid, 2, 1);
    for (source, body) in &initial {
        manager.submit(&vivid, 0, *source, 1, Arc::clone(body));
        manager.submit(&vivid, 0, *source, 1, Arc::clone(body));
    }
    assert_eq!(manager.pending.len(), 2);
    assert_eq!(
        manager.bytes,
        initial.iter().map(|(_, body)| body.len()).sum::<usize>()
    );
    assert!(manager.copies.is_empty());
    manager.byte_limit = 0;
    windows[0].submit(Canvas::new()).unwrap();
    let fresh = frames(&vivid, 1, 2);
    manager.submit(&vivid, 0, fresh[0].0, 2, Arc::clone(&fresh[0].1));
    assert_eq!(
        manager.pending.len(),
        1,
        "only the updating owner lost its queued body"
    );
    assert!(manager.replay.contains(&fresh[0].0));
    manager.reset(&vivid);
    assert_eq!(manager.bytes, 0);
    assert!(manager.pending.is_empty());
    a.close().unwrap();
    b.close().unwrap();
    vivid.shutdown();
}
