//! Original action claims bind exact clients, bridges and SDK request lifetimes.

// Rust guideline compliant 2026-10-07
use super::*;
use std::{
    collections::HashSet,
    time::{Duration, Instant},
};
use vivid_protocol::{
    cbor::Value,
    messages,
    overlay::{
        WindowOptions,
        wire::{Action, Query, SetWindow, WindowAction, WindowAddress},
    },
    scene::Fit,
    track::{KindConfiguration, TrackMode},
};
use vivid_sdk::{
    ProducerAuthentication, ProducerConfig, RequestMetadata, SceneNode, Session, Surface,
    SurfaceBuilder, SurfaceRole, Track, TrackChannel, TrackConfiguration, VectorConfiguration,
};

struct Producer {
    session: Session,
    surface: Surface,
    track: Track,
    _channel: TrackChannel,
    _lane: vivid_sdk::OverlayInputLane,
    node: SceneNode,
}
impl Producer {
    fn new(vivid: &VirtualVivid, pane: u64) -> Self {
        vivid.update_metrics(pane, 100, 40, (8, 16));
        let secret = vivid.issue_pane_capability(pane).unwrap();
        let endpoint = vivid.endpoint();
        let mut config = ProducerConfig {
            authentication: ProducerAuthentication::root_hex(&secret).unwrap(),
            endpoint_control: Some(endpoint.clone()),
            endpoint_interactive: Some(endpoint.clone()),
            endpoint_realtime: Some(endpoint.clone()),
            endpoint_bulk: Some(endpoint),
            required_profiles: [
                vivid_sdk::CORE_CONTROL,
                vivid_sdk::LIVE_MEDIA,
                vivid_sdk::OBSERVABILITY,
                vivid_sdk::TERMINAL_SURFACE,
                vivid_sdk::TERMINAL_OVERLAY,
                vivid_sdk::VECTOR_SCENE,
                vivid_sdk::OVERLAY_INPUT,
            ]
            .map(str::to_owned)
            .to_vec(),
            optional_profiles: Vec::new(),
            ..ProducerConfig::default()
        };
        config.required_profiles.sort();
        let mut session = Session::connect(config).unwrap();
        let lane = session.open_overlay_input_lane(1).unwrap();
        lane.renew(5_000_000, Duration::from_secs(1)).unwrap();
        let definition = SurfaceBuilder::new(&session, 32, 32)
            .unwrap()
            .titled(SurfaceRole::Figure, "action owner")
            .build()
            .unwrap();
        let surface = session
            .create_surface(definition, &RequestMetadata::default())
            .unwrap();
        let track = session
            .create_track(
                TrackConfiguration {
                    direction: vivid_sdk::TrackDirection::default(),
                    context_id: surface.context_id(),
                    surface_id: surface.id(),
                    track_id: session.allocate_id().unwrap(),
                    slot: vivid_sdk::SLOT_VECTOR,
                    mode: TrackMode::Live,
                    lane: messages::LaneClass::Bulk,
                    maximum_record_body: 65536,
                    maximum_rate_millihertz: 60_000,
                    maximum_encoded_bits_per_second: 65536 * 8 * 60,
                    maximum_records_per_second: 60,
                    maximum_inflight_body_bytes: 65536,
                    target_latency_us: 0,
                    maximum_latency_us: 0,
                    retained_pixel_charge: 32 * 32,
                    kind: KindConfiguration::VectorScene(VectorConfiguration {
                        width: 32,
                        height: 32,
                        maximum_scene_bytes: 65524,
                    }),
                },
                &RequestMetadata::default(),
            )
            .unwrap();
        let channel = session.open_track_channel(&track).unwrap();
        let address = WindowAddress {
            context_id: surface.context_id(),
            surface_id: surface.id(),
            generation: surface.generation().get(),
        };
        session
            .set_overlay_window(&SetWindow {
                address,
                expected_revision: 0,
                options: WindowOptions::new(
                    vivid_protocol::vector::Rect::new(0., 0., 32., 32.).unwrap(),
                    vivid_protocol::overlay::WindowMode::Floating,
                ),
            })
            .unwrap();
        let node = SceneNode {
            owning_context_id: surface.context_id(),
            node_id: 1,
            surface_context_id: surface.context_id(),
            surface_id: surface.id(),
            geometry: vec![
                (0, Value::Unsigned(1)),
                (1, Value::Unsigned(0)),
                (2, Value::Unsigned(0)),
                (3, Value::Unsigned(1 << 32)),
                (4, Value::Unsigned(1 << 32)),
                (5, Value::Unsigned(0)),
            ],
            fit: Fit::Contain,
            linear_sampling: true,
            z_index: 0,
            visible: true,
            opacity: u16::MAX,
            clip: None,
        };
        session
            .create_node(&node, &RequestMetadata::default())
            .unwrap();
        Self {
            session,
            surface,
            track,
            _channel: channel,
            _lane: lane,
            node,
        }
    }
    fn action(&self) -> (vivid_sdk::overlay::OverlayHostHandle, Action) {
        let status = self
            .session
            .query_overlay_window(Query {
                context_id: self.surface.context_id(),
                surface_id: self.surface.id(),
            })
            .unwrap();
        (
            self.session.overlay_host_handle().unwrap(),
            Action {
                address: status.window.address,
                expected_revision: status.window.expected_revision,
                action: WindowAction::Center,
            },
        )
    }
}
fn fixture() -> (VirtualVivid, WindowActions) {
    let vivid = VirtualVivid::start_configured_eventless(
        vivid_sdk::presenter::SocketListener::bind("tcp:127.0.0.1:0").unwrap(),
        vivid_sdk::presenter::PresenterConfig::terminal_with_overlay(
            vivid_sdk::presenter::MediaConfig::default(),
        ),
    )
    .unwrap();
    vivid.enable_overlay_host_relay();
    let manager = WindowActions::new(vivid.attach_overlay_action_relay().unwrap());
    (vivid, manager)
}
fn request(vivid: &VirtualVivid) -> OverlayHostRequest {
    let until = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(request) = vivid.take_overlay_host_requests().into_iter().next() {
            assert_eq!(request.record_type, messages::OVERLAY_ACTION);
            return request;
        }
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(1));
    }
}
fn success() -> Vec<u8> {
    messages::Envelope::new(0, Vec::new()).encode().unwrap()
}

#[test]
fn replacement_rejects_claimed_actions_and_late_replies_while_a_peer_is_pending() {
    let (vivid, mut manager) = fixture();
    let first = Producer::new(&vivid, 1);
    let mut peer = Producer::new(&vivid, 2);
    assert_eq!(first.action().1.address, peer.action().1.address);
    let writer = crate::ipc::test_shared_writer(Box::new(std::io::sink()));
    for kind in [
        WindowAction::Center,
        WindowAction::Close,
        WindowAction::Focus,
        WindowAction::Raise,
        WindowAction::Lower,
    ] {
        let (handle, mut action) = first.action();
        action.action = kind;
        manager.bind(&vivid, 7, 10);
        std::thread::scope(|scope| {
            let caller = scope.spawn(move || {
                handle(
                    messages::OVERLAY_ACTION,
                    action.address.surface_id,
                    action.payload().unwrap(),
                )
            });
            let original = request(&vivid);
            let id = original.id;
            manager.enqueue(&vivid, 7, original);
            manager.service(&vivid, Some((7, 10, Arc::clone(&writer))));
            assert!(manager.claim(7, 10, id).0);
            assert_eq!(manager.pending_counts(), (1, 1));
            peer.node.z_index += 1;
            peer.session
                .update_node(&peer.node, &RequestMetadata::default())
                .unwrap();
            let snapshot = vivid.projection_snapshot(&HashSet::from([2]));
            assert_eq!(snapshot.nodes.len(), 1);
            assert_eq!(snapshot.nodes[0].producer, peer.session.info().session_id);
            assert_eq!(snapshot.nodes[0].config.node.z_index, peer.node.z_index);

            manager.bind(&vivid, 8, 11);
            caller.join().unwrap().unwrap_err();
            assert_eq!(manager.pending_counts(), (0, 0));
            assert_eq!(first.action().1.expected_revision, action.expected_revision);
            let (handle, fresh) = peer.action();
            let peer_caller = scope.spawn(move || {
                handle(
                    messages::OVERLAY_ACTION,
                    fresh.address.surface_id,
                    fresh.payload().unwrap(),
                )
            });
            let pending = request(&vivid);
            let peer_id = pending.id;
            assert_ne!(id, peer_id);
            manager.enqueue(&vivid, 8, pending);
            manager.service(&vivid, Some((8, 11, Arc::clone(&writer))));
            assert!(manager.claim(8, 11, peer_id).0);
            // A successful physical reply can reach the actor after replacement.
            // Neither its stale correlation nor the old client resolves the peer.
            for (client, bridge, old_id) in [(7, 10, id), (8, 11, id), (7, 10, peer_id)] {
                assert!(!manager.reply(&vivid, client, bridge, old_id, Ok(success())));
            }
            assert_eq!(first.action().1.expected_revision, action.expected_revision);
            assert_eq!(manager.pending_counts(), (1, 1));
            assert!(manager.reply(&vivid, 8, 11, peer_id, Ok(success())));
            peer_caller.join().unwrap().unwrap();
            assert_eq!(
                peer.action().1.expected_revision,
                fresh.expected_revision + 1
            );
            assert_eq!(manager.pending_counts(), (0, 0));
        });
    }
    first.session.close().unwrap();
    peer.session.close().unwrap();
}

#[test]
fn window_actions_require_exact_client_bridge_original_and_one_shot_claim() {
    let (vivid, mut manager) = fixture();
    let a = Producer::new(&vivid, 1);
    let mut b = Producer::new(&vivid, 2);
    assert_eq!(
        (a.surface.context_id(), a.surface.id(), a.track.id()),
        (b.surface.context_id(), b.surface.id(), b.track.id())
    );
    let (handle, action) = a.action();
    std::thread::scope(|scope| {
        let caller = scope.spawn(move || {
            handle(
                messages::OVERLAY_ACTION,
                action.address.surface_id,
                action.payload().unwrap(),
            )
        });
        let request = request(&vivid);
        let id = request.id;
        let original = manager.route.request(&request);
        manager.enqueue(&vivid, 7, request);
        manager.bind(&vivid, 7, 10);
        manager.pending.get_mut(&id).unwrap().bridge = Some(10);
        let lifetime = manager.pending[&id].receipt.remaining_lifetime().unwrap();
        assert!(lifetime <= Duration::from_secs(10));
        for (client, bridge, request) in [(8, 10, id), (7, 11, id), (7, 10, id + 1)] {
            assert_eq!(manager.claim(client, bridge, request), (false, 0));
            assert!(!manager.reply(&vivid, client, bridge, request, Ok(success())));
        }
        assert!(!caller.is_finished());
        let (allowed, remaining) = manager.claim(7, 10, id);
        assert!(allowed && remaining > 0 && remaining <= 10_000_000);
        assert_eq!(manager.claim(7, 10, id), (false, 0));
        assert!(manager.pending[&id].receipt.remaining_lifetime().unwrap() <= lifetime);
        assert!(manager.reply(&vivid, 7, 10, id, Ok(success())));
        caller.join().unwrap().unwrap();
        assert!(original.remaining_lifetime().is_none());
        assert!(!manager.reply(&vivid, 7, 10, id, Ok(success())));
    });
    a.session.close().unwrap();
    let until = Instant::now() + Duration::from_secs(3);
    loop {
        let snapshot = vivid.projection_snapshot(&HashSet::from([1, 2]));
        if snapshot.sources.len() == 1 {
            assert_eq!(snapshot.nodes.len(), 1);
            assert_eq!(snapshot.nodes[0].producer, b.session.info().session_id);
            break;
        }
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(1));
    }
    b.node.z_index = 4;
    b.session
        .update_node(&b.node, &RequestMetadata::default())
        .unwrap();
    assert_eq!(
        vivid.projection_snapshot(&HashSet::from([2])).nodes[0]
            .config
            .node
            .z_index,
        4
    );
    let (handle, action) = b.action();
    std::thread::scope(|scope| {
        let caller = scope.spawn(move || {
            handle(
                messages::OVERLAY_ACTION,
                action.address.surface_id,
                action.payload().unwrap(),
            )
        });
        let request = request(&vivid);
        let id = request.id;
        manager.enqueue(&vivid, 7, request);
        manager.pending.get_mut(&id).unwrap().bridge = Some(10);
        manager.reset(&vivid);
        assert_eq!(manager.claim(7, 10, id), (false, 0));
        caller.join().unwrap().unwrap_err();
    });
    b.node.z_index = 5;
    b.session
        .update_node(&b.node, &RequestMetadata::default())
        .unwrap();
    assert_eq!(
        vivid.projection_snapshot(&HashSet::from([2])).nodes[0]
            .config
            .node
            .z_index,
        5
    );
    b.session.close().unwrap();
}
