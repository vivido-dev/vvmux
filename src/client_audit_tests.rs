// Positive regressions for the vvmux robustness audit.
#[test]
fn ipc_writer_rejects_calls_after_partial_record_failure() {
    struct FailOnce {
        calls: usize,
    }
    impl std::io::Write for FailOnce {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.calls += 1;
            if self.calls == 2 {
                return Err(io::Error::other("injected body failure"));
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let writer = crate::ipc::test_shared_writer(Box::new(FailOnce { calls: 0 }));
    let mut writer = writer.lock().unwrap();
    assert!(writer.write_raw(1, 0, &[1, 2, 3]).is_err());
    assert!(writer.write_raw(1, 0, &[4, 5, 6]).is_err());
}

#[test]
fn total_media_queue_has_source_and_byte_limits() {
    let mut queue = TrackMediaQueues::default();
    for producer in 1..=BRIDGE_QUEUE_SOURCES as u64 {
        assert!(
            queue
                .push(
                    BridgeMedia {
                        generation: 1,
                        delivery_id: producer,
                        source: BridgeSourceKey {
                            producer,
                            context: 1,
                            surface: 1,
                            track: 1
                        },
                        record_type: vivid_protocol::messages::RASTER_FRAME,
                        offset: 0,
                        total: 1024,
                        last: true,
                        bytes: vec![0; 1024],
                    },
                    1
                )
                .is_ok()
        );
    }
    let media = BridgeMedia {
        generation: 1,
        delivery_id: 9999,
        source: BridgeSourceKey {
            producer: 9999,
            context: 1,
            surface: 1,
            track: 1,
        },
        record_type: vivid_protocol::messages::RASTER_FRAME,
        offset: 0,
        total: 1,
        last: true,
        bytes: vec![0],
    };
    assert!(queue.push(media, 1).is_err());
    while queue.pop().is_some() {}
    assert_eq!(queue.bytes, 0);
    assert_eq!(queue.chunks, 0);
    let mut bytes = Vec::new();
    bytes.reserve_exact(BRIDGE_QUEUE_BYTES + 1);
    let media = BridgeMedia {
        generation: 1,
        delivery_id: 1,
        source: BridgeSourceKey {
            producer: 1,
            context: 1,
            surface: 1,
            track: 1,
        },
        record_type: vivid_protocol::messages::RASTER_FRAME,
        offset: 0,
        total: 1,
        last: true,
        bytes,
    };
    assert!(queue.push(media, 1).is_err());
}

#[test]
fn queued_ipc_send_does_not_wait_for_client_writer() {
    use std::sync::Condvar;
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let (entered, entry) = mpsc::channel();
    struct Blocked {
        gate: Arc<(Mutex<bool>, Condvar)>,
        entered: mpsc::Sender<()>,
    }
    impl std::io::Write for Blocked {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let _ = self.entered.send(());
            let (lock, changed) = &*self.gate;
            let mut released = lock.lock().unwrap();
            while !*released {
                released = changed.wait(released).unwrap();
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let writer = crate::ipc::test_shared_writer(Box::new(Blocked {
        gate: gate.clone(),
        entered,
    }));
    writer.lock().unwrap().enable_queued_output().unwrap();
    let _keep_writer = writer.clone();
    let (done, finished) = mpsc::channel();
    let worker = thread::spawn(move || {
        crate::ipc::send(&writer, &ServerMessage::Bell).unwrap();
        done.send(()).unwrap();
    });
    entry.recv_timeout(Duration::from_secs(1)).unwrap();
    let blocked = finished.recv_timeout(Duration::from_millis(100)).is_err();
    *gate.0.lock().unwrap() = true;
    gate.1.notify_all();
    worker.join().unwrap();
    assert!(!blocked);
}

#[test]
#[cfg(unix)]
fn client_encoder_preserves_microphone_framing() {
    let (a, b) = UnixStream::pair().unwrap();
    let server = thread::spawn(move || {
        crate::ipc::establish(test_transport(a), ChannelKind::Control).unwrap()
    });
    let (_reader, writer) = crate::ipc::establish(test_transport(b), ChannelKind::Control).unwrap();
    let (mut reader, _writer) = server.join().unwrap();
    // SessionAdapter and the native client must both use the typed encoder.
    writer
        .lock()
        .unwrap()
        .send_client(&ClientMessage::Microphone {
            bridge_instance_id: 1,
            source: BridgeSourceKey {
                producer: 1,
                context: 1,
                surface: 1,
                track: 1,
            },
            generation: 1,
            bytes: vec![],
        })
        .unwrap();
    assert!(matches!(
        reader.recv_client().unwrap(),
        ClientMessage::Microphone { .. }
    ));
}

#[test]
fn bridge_drop_cancels_blocked_outer_request() {
    let presenter = vivid_sdk::testing::TestPresenter::start(80, 24).unwrap();
    let bridge = crate::bridge::OuterBridge::connect(
        presenter.endpoint().into(),
        Zeroizing::new(vivid_sdk::testing::ROOT_SECRET_HEX.into()),
        DisplayMetrics::default(),
    )
    .unwrap();
    presenter
        .script()
        .drop_reply(vivid_protocol::messages::SURFACE_READY, 1);
    let mut worker =
        BridgeWorker::spawn_with_sender(bridge, BridgeClientSender::new(|_| Ok(())), 8).unwrap();
    let key = BridgeSourceKey {
        producer: 1,
        context: 1,
        surface: 1,
        track: 1,
    };
    worker.replace_snapshot(BridgeSnapshot {
        microphones: vec![],
        generation: 0,
        virtual_revision: 1,
        surfaces: vec![test_surface(key)],
        tracks: vec![],
        nodes: vec![],
        videos_needing_keyframes: vec![],
    });
    let deadline = Instant::now() + Duration::from_secs(3);
    while !presenter
        .observed()
        .iter()
        .any(|r| r.record_type == vivid_protocol::messages::CREATE_SURFACE)
    {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    let (done, finished) = mpsc::channel();
    let dropping = thread::spawn(move || {
        drop(worker);
        done.send(()).unwrap();
    });
    let blocked = finished.recv_timeout(Duration::from_millis(150)).is_err();
    drop(presenter); // Release the fault before joining, so the probe is bounded.
    dropping.join().unwrap();
    assert!(!blocked);
}
