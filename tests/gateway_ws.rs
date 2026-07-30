#![cfg(all(unix, feature = "server-capability"))]

use std::fs;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use vivid_protocol::auth::{self, Secret32};
use vivid_protocol::cbor::Value;
use vivid_protocol::messages::{self, Hello, HelloAuthentication, Welcome, WelcomeAuthentication};
use vivid_protocol::resource::{Resource, ResourceContract};
use vivid_protocol::wire::{ConnectionKind, HEADER_SIZE, PREFACE_SIZE, Preface, RecordHeader};

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn private_directory(path: &Path) {
    fs::create_dir_all(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn command(executable: &Path, runtime: &Path, config: &Path) -> Command {
    let mut command = Command::new(executable);
    command
        .env("XDG_RUNTIME_DIR", runtime)
        .env("XDG_CONFIG_HOME", config)
        .env_remove("VIVID_ENDPOINT")
        .env_remove("VIVID_ENDPOINT_CONTROL")
        .env_remove("VIVID_ENDPOINT_REALTIME")
        .env_remove("VIVID_ENDPOINT_BULK")
        .env_remove("VIVID_TOKEN")
        .env_remove("VIVID_ROOT_SECRET")
        .env_remove("VIVID_SSH_ENDPOINT")
        .env_remove("VIVID_SSH_TOKEN");
    command
}

fn websocket_request(url: &str, origin: &str) -> tokio_tungstenite::tungstenite::http::Request<()> {
    let mut request = url.into_client_request().unwrap();
    request
        .headers_mut()
        .insert("Origin", HeaderValue::from_str(origin).unwrap());
    request.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        HeaderValue::from_static("vvmux.v1"),
    );
    request
}

fn vivid_request(
    url: &str,
    origin: &str,
    connection: &str,
    token: &str,
    kind: u8,
) -> tokio_tungstenite::tungstenite::http::Request<()> {
    let mut request = url.into_client_request().unwrap();
    request
        .headers_mut()
        .insert("Origin", HeaderValue::from_str(origin).unwrap());
    request.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        HeaderValue::from_str(&format!(
            "vvmux.vivid.v1, vvmux.connection.{connection}, vvmux.auth.{token}, vvmux.kind.{kind}"
        ))
        .unwrap(),
    );
    request
}

#[test]
fn authenticated_gateway_creates_lists_attaches_and_drives_a_session() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let directory = tempfile::Builder::new()
            .prefix("vvmux-gw-")
            .tempdir_in("/tmp")
            .unwrap();
        let runtime_root = directory.path().join("runtime");
        let config_root = directory.path().join("config");
        private_directory(&runtime_root);
        private_directory(&config_root);
        let config_directory = config_root.join("vvmux");
        private_directory(&config_directory);
        fs::write(
            config_directory.join("config.toml"),
            "[general]\nrender_interval_ms = 1\n\n[server]\noutbound_queue_bytes = 524288\n",
        )
        .unwrap();
        let auth_file = config_root.join("server-auth.json");
        let executable = Path::new(env!("CARGO_BIN_EXE_vvmux"));

        let token_output = command(executable, &runtime_root, &config_root)
            .args(["token", "create", "--auth-file"])
            .arg(&auth_file)
            .output()
            .unwrap();
        assert!(
            token_output.status.success(),
            "token creation failed: {}",
            String::from_utf8_lossy(&token_output.stderr)
        );
        let token = String::from_utf8(token_output.stdout).unwrap();
        let token = token.trim();
        assert_eq!(token.len(), 43);

        let reserved = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = reserved.local_addr().unwrap();
        drop(reserved);
        let child = command(executable, &runtime_root, &config_root)
            .args(["serve", "--listen"])
            .arg(address.to_string())
            .args(["--allow-origin", "http://127.0.0.1:3000", "--auth-file"])
            .arg(&auth_file)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let _gateway = ChildGuard(child);
        let url = format!("ws://{address}/v1/ws");

        let mut connection = None;
        for _ in 0..100 {
            match tokio_tungstenite::connect_async(websocket_request(&url, "http://127.0.0.1:3000"))
                .await
            {
                Ok((socket, response)) => {
                    assert_eq!(
                        response
                            .headers()
                            .get("Sec-WebSocket-Protocol")
                            .and_then(|value| value.to_str().ok()),
                        Some("vvmux.v1")
                    );
                    connection = Some(socket);
                    break;
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
        let mut socket = connection.expect("gateway did not start");

        assert!(
            tokio_tungstenite::connect_async(websocket_request(&url, "http://malicious.example",))
                .await
                .is_err(),
            "an unlisted browser origin was accepted"
        );

        let (mut wrong_token, _) =
            tokio_tungstenite::connect_async(websocket_request(&url, "http://127.0.0.1:3000"))
                .await
                .unwrap();
        wrong_token
            .send(Message::Text(
                serde_json::json!({
                    "type": "hello",
                    "protocol": 1,
                    "token": "A".repeat(43),
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        assert!(matches!(
            wrong_token.next().await,
            Some(Ok(Message::Close(_))) | Some(Err(_)) | None
        ));

        let (mut wrong_version, _) =
            tokio_tungstenite::connect_async(websocket_request(&url, "http://127.0.0.1:3000"))
                .await
                .unwrap();
        wrong_version
            .send(Message::Text(
                serde_json::json!({"type": "hello", "protocol": 2, "token": token})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        assert!(matches!(
            wrong_version.next().await,
            Some(Ok(Message::Close(_))) | Some(Err(_)) | None
        ));

        let (mut pre_auth_request, _) =
            tokio_tungstenite::connect_async(websocket_request(&url, "http://127.0.0.1:3000"))
                .await
                .unwrap();
        pre_auth_request
            .send(Message::Text(
                r#"{"type":"list_sessions","request_id":99}"#.into(),
            ))
            .await
            .unwrap();
        assert!(matches!(
            pre_auth_request.next().await,
            Some(Ok(Message::Close(_))) | Some(Err(_)) | None
        ));

        let (mut oversized, _) =
            tokio_tungstenite::connect_async(websocket_request(&url, "http://127.0.0.1:3000"))
                .await
                .unwrap();
        let oversized_send = oversized
            .send(Message::Text("x".repeat(1024 * 1024 + 1).into()))
            .await;
        if oversized_send.is_ok() {
            let oversized_result = tokio::time::timeout(Duration::from_secs(3), oversized.next())
                .await
                .expect("oversized authentication frame was not rejected");
            assert!(matches!(
                oversized_result,
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None
            ));
        }

        socket
            .send(Message::Text(
                serde_json::json!({"type": "hello", "protocol": 1, "token": token})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        let hello = socket.next().await.unwrap().unwrap();
        let Message::Text(hello) = hello else {
            panic!("expected hello control message")
        };
        let hello: serde_json::Value = serde_json::from_str(&hello).unwrap();
        assert_eq!(hello["type"], "hello");
        assert!(hello["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "vivid-bridge-v1"));
        let vivid_access = &hello["vivid"];
        assert_eq!(vivid_access["wire_version"], "1.5");
        let vivid_url = format!("ws://{address}{}", vivid_access["endpoint"].as_str().unwrap());
        assert!(
            tokio_tungstenite::connect_async(vivid_request(
                &vivid_url,
                "http://malicious.example",
                vivid_access["connection"].as_str().unwrap(),
                vivid_access["token"].as_str().unwrap(),
                ConnectionKind::Control as u8,
            ))
            .await
            .is_err(),
            "an unlisted origin opened a Vivid transport"
        );
        assert!(
            tokio_tungstenite::connect_async(vivid_request(
                &vivid_url,
                "http://127.0.0.1:3000",
                vivid_access["connection"].as_str().unwrap(),
                &"A".repeat(43),
                ConnectionKind::Control as u8,
            ))
            .await
            .is_err(),
            "an invalid ephemeral token opened a Vivid transport"
        );

        let (mut auth_timeout, _) =
            tokio_tungstenite::connect_async(websocket_request(&url, "http://127.0.0.1:3000"))
                .await
                .unwrap();
        let timeout_result = tokio::time::timeout(Duration::from_secs(7), auth_timeout.next())
            .await
            .expect("unauthenticated connection exceeded the authentication deadline");
        assert!(matches!(
            timeout_result,
            Some(Ok(Message::Close(_))) | Some(Err(_)) | None
        ));

        let session = format!("gateway-{}", std::process::id());
        let (mut vivid_socket, vivid_response) = tokio_tungstenite::connect_async(vivid_request(
            &vivid_url,
            "http://127.0.0.1:3000",
            vivid_access["connection"].as_str().unwrap(),
            vivid_access["token"].as_str().unwrap(),
            ConnectionKind::Control as u8,
        ))
        .await
        .unwrap();
        assert_eq!(
            vivid_response
                .headers()
                .get("Sec-WebSocket-Protocol")
                .and_then(|value| value.to_str().ok()),
            Some("vvmux.vivid.v1")
        );

        socket
            .send(Message::Text(
                serde_json::json!({
                    "type": "create_session",
                    "request_id": 1,
                    "name": session,
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        let created = socket.next().await.unwrap().unwrap();
        assert!(
            matches!(created, Message::Text(ref text) if text.contains("created")),
            "unexpected create response: {created:?}"
        );

        socket
            .send(Message::Text(
                r#"{"type":"list_sessions","request_id":2}"#.into(),
            ))
            .await
            .unwrap();
        let listed = socket.next().await.unwrap().unwrap();
        assert!(matches!(listed, Message::Text(ref text) if text.contains(&session)));

        socket
            .send(Message::Text(
                serde_json::json!({
                    "type": "attach",
                    "request_id": 3,
                    "name": session,
                    "display": {
                        "columns": 80,
                        "rows": 24,
                        "cell_width": 8,
                        "cell_height": 16,
                    },
                    "vivid": true,
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();

        let mut vivid_bytes = Vec::new();
        let (hello_header, hello_body) = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let message = vivid_socket.next().await.unwrap().unwrap();
                if let Message::Binary(bytes) = message {
                    vivid_bytes.extend_from_slice(&bytes);
                }
                if vivid_bytes.len() < PREFACE_SIZE + HEADER_SIZE {
                    continue;
                }
                let preface =
                    Preface::decode(vivid_bytes[..PREFACE_SIZE].try_into().unwrap()).unwrap();
                assert_eq!(preface.kind, ConnectionKind::Control);
                let header = RecordHeader::decode(
                    vivid_bytes[PREFACE_SIZE..PREFACE_SIZE + HEADER_SIZE]
                        .try_into()
                        .unwrap(),
                );
                let end = PREFACE_SIZE + HEADER_SIZE + header.body_length as usize;
                if vivid_bytes.len() >= end {
                    break (
                        header,
                        vivid_bytes[PREFACE_SIZE + HEADER_SIZE..end].to_vec(),
                    );
                }
            }
        })
        .await
        .expect("Vivid HELLO timed out");
        assert_eq!(hello_header.record_type, messages::HELLO);
        let (hello_request, vivid_hello) = Hello::decode(&hello_body).unwrap();
        let route_secret = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(vivid_access["token"].as_str().unwrap())
            .unwrap();
        let root_secret = Secret32::new(route_secret.try_into().unwrap());
        let authless = vivid_hello.authless_payload().unwrap();
        let HelloAuthentication::Root { proof } = &vivid_hello.authentication else {
            panic!("gateway bridge did not use Vivid root authentication")
        };
        assert!(auth::verify_root_hello_proof(
            &root_secret,
            vivid_bytes[..PREFACE_SIZE].try_into().unwrap(),
            &authless,
            proof,
        ));
        let server_nonce = [2; auth::NONCE_BYTES];
        let session_tag = [1; messages::SESSION_TAG_BYTES];
        let prk = auth::extract_handshake_prk(
            &root_secret,
            &vivid_hello.client_nonce,
            &server_nonce,
            &[0; 32],
        );
        let mut contract = ResourceContract::denied();
        for resource in [
            Resource::Surfaces,
            Resource::Tracks,
            Resource::Nodes,
            Resource::VideoTracks,
            Resource::AudioTracks,
            Resource::RasterTracks,
            Resource::ImageTracks,
            Resource::DecoderInstances,
            Resource::TrackConnections,
            Resource::PendingRequests,
            Resource::RegisteredWaits,
            Resource::IdempotencyEntries,
            Resource::ObservationQueueEntries,
            Resource::OpenSceneTransactions,
            Resource::PendingChannelOpenAttempts,
            Resource::ActiveTerminalAnchors,
            Resource::SeenTerminalAnchorIds,
        ] {
            contract.set(resource, 64);
        }
        contract.set(Resource::CodedPixelsPerTrack, 8192 * 8192);
        contract.set(Resource::DecodedPixelsPerSecond, 8192 * 8192 * 60);
        contract.set(Resource::EncodedBitsPerSecond, 1_000_000_000);
        contract.set(Resource::MediaRecordsPerSecond, 4_000);
        contract.set(Resource::AudioSampleRate, 192_000);
        contract.set(Resource::AudioChannelsPerTrack, 8);
        contract.set(Resource::InflightMediaBytes, 256 * 1024 * 1024);
        contract.set(Resource::RetainedPixels, 8192 * 8192);
        contract.set(
            Resource::MediaRecordBody,
            u64::from(vivid_protocol::HARD_MAX_RECORD_BODY),
        );
        contract.set(
            Resource::ControlRecordBody,
            u64::from(vivid_protocol::CONTROL_MAX_RECORD_BODY),
        );
        contract.set(Resource::ImageCacheBytes, 64 * 1024 * 1024);
        let mut welcome = Welcome {
            session_id: 1,
            session_tag,
            root_context_id: 2,
            target_generation: 1,
            target_profile: vivid_protocol::registry::TERMINAL_SURFACE.into(),
            target_descriptor: vec![
                (0, Value::Unsigned(640)),
                (1, Value::Unsigned(384)),
                (2, Value::Unsigned(80)),
                (3, Value::Unsigned(24)),
                (4, Value::Unsigned(8)),
                (5, Value::Unsigned(16)),
                (6, Value::Bool(true)),
                (7, Value::Unsigned(3)),
                (8, Value::Unsigned(64)),
            ],
            accepted_profiles: vivid_hello.required_profiles.clone(),
            maximum_control_body: vivid_hello.maximum_control_body,
            server_nonce,
            authentication: WelcomeAuthentication {
                kind: messages::AUTHENTICATION_ROOT,
                confirmation: [0; 32],
                lease_state: 0,
                activation_attempt_status: 0,
            },
            session_revision: 1,
            scene_revision: 0,
            resource_contract: contract,
            establishment_state: 0,
            resume_generation: 0,
            extensions: vec![],
        };
        welcome.confirm(&prk).unwrap();
        let welcome_body = welcome.encode(hello_request).unwrap();
        let welcome_header = RecordHeader {
            body_length: welcome_body.len().try_into().unwrap(),
            record_type: messages::WELCOME,
            flags: 0,
            object_id: 0,
            sequence: 1,
        };
        let mut welcome = welcome_header.encode().to_vec();
        welcome.extend_from_slice(&welcome_body);
        vivid_socket
            .send(Message::Binary(welcome.into()))
            .await
            .unwrap();

        let attached = socket.next().await.unwrap().unwrap();
        assert!(
            matches!(attached, Message::Text(ref text) if text.contains(r#""type":"attached""#) && text.contains(r#""text_only":false"#))
        );
        assert!(matches!(
            socket.next().await.unwrap().unwrap(),
            Message::Binary(_)
        ));

        socket
            .send(Message::Text(
                r#"{"type":"list_sessions","request_id":77}"#.into(),
            ))
            .await
            .unwrap();
        let invalid_state = tokio::time::timeout(Duration::from_secs(3), async {
            while let Some(message) = socket.next().await {
                if matches!(message.unwrap(), Message::Text(ref text) if text.contains("invalid_state"))
                {
                    return true;
                }
            }
            false
        })
        .await
        .unwrap();
        assert!(invalid_state, "attached management request was accepted");

        socket
            .send(Message::Binary(b"printf VVWS_GATEWAY_OK\r".to_vec().into()))
            .await
            .unwrap();
        let output = tokio::time::timeout(Duration::from_secs(5), async {
            let mut output = Vec::new();
            while let Some(message) = socket.next().await {
                if let Message::Binary(bytes) = message.unwrap() {
                    output.extend_from_slice(&bytes);
                    if output
                        .windows(b"VVWS_GATEWAY_OK".len())
                        .any(|window| window == b"VVWS_GATEWAY_OK")
                    {
                        return output;
                    }
                }
            }
            output
        })
        .await
        .expect("terminal output timed out");
        assert!(
            output
                .windows(b"VVWS_GATEWAY_OK".len())
                .any(|window| window == b"VVWS_GATEWAY_OK")
        );

        socket
            .send(Message::Text(
                serde_json::json!({
                    "type": "resize",
                    "display": {
                        "columns": 100,
                        "rows": 30,
                        "cell_width": 8,
                        "cell_height": 16,
                    },
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        let resized = tokio::time::timeout(Duration::from_secs(3), async {
            while let Some(message) = socket.next().await {
                if let Message::Binary(bytes) = message.unwrap()
                    && !bytes.is_empty()
                {
                    return true;
                }
            }
            false
        })
        .await
        .unwrap();
        assert!(resized, "resize did not produce a fresh terminal frame");

        let (mut takeover, _) =
            tokio_tungstenite::connect_async(websocket_request(&url, "http://127.0.0.1:3000"))
                .await
                .unwrap();
        takeover
            .send(Message::Text(
                serde_json::json!({"type": "hello", "protocol": 1, "token": token})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        assert!(matches!(
            takeover.next().await.unwrap().unwrap(),
            Message::Text(ref text) if text.contains("hello")
        ));
        let attach = |request_id, takeover: bool| {
            Message::Text(
                serde_json::json!({
                    "type": "attach",
                    "request_id": request_id,
                    "name": session,
                    "display": {
                        "columns": 80,
                        "rows": 24,
                        "cell_width": 8,
                        "cell_height": 16,
                    },
                    "takeover": takeover,
                })
                .to_string()
                .into(),
            )
        };
        takeover.send(attach(4, false)).await.unwrap();
        assert!(matches!(
            takeover.next().await.unwrap().unwrap(),
            Message::Text(ref text) if text.contains("session_occupied")
        ));
        takeover.send(attach(5, true)).await.unwrap();
        assert!(matches!(
            takeover.next().await.unwrap().unwrap(),
            Message::Text(ref text) if text.contains(r#""type":"attached""#)
        ));
        assert!(matches!(
            takeover.next().await.unwrap().unwrap(),
            Message::Binary(_)
        ));
        let full_redraw = tokio::time::timeout(Duration::from_secs(3), async {
            while let Some(message) = takeover.next().await {
                if let Message::Binary(bytes) = message.unwrap()
                    && !bytes.is_empty()
                {
                    return true;
                }
            }
            false
        })
        .await
        .unwrap();
        assert!(
            full_redraw,
            "replacement attachment did not receive a full render"
        );

        let replaced = tokio::time::timeout(Duration::from_secs(3), async {
            while let Some(message) = socket.next().await {
                if matches!(message.unwrap(), Message::Text(ref text) if text.contains("detached"))
                {
                    return true;
                }
            }
            false
        })
        .await
        .unwrap();
        assert!(replaced);

        takeover
            .send(Message::Text(r#"{"type":"detach"}"#.into()))
            .await
            .unwrap();
        let detached = tokio::time::timeout(Duration::from_secs(3), async {
            while let Some(message) = takeover.next().await {
                if matches!(message.unwrap(), Message::Text(ref text) if text.contains("detached"))
                {
                    return true;
                }
            }
            false
        })
        .await
        .unwrap();
        assert!(detached);

        takeover.send(attach(6, false)).await.unwrap();
        assert!(matches!(
            takeover.next().await.unwrap().unwrap(),
            Message::Text(ref text) if text.contains(r#""type":"attached""#)
        ));
        assert!(matches!(
            takeover.next().await.unwrap().unwrap(),
            Message::Binary(_)
        ));
        takeover
            .send(Message::Binary(
                b"printf VVWS_RECONNECTED\r".to_vec().into(),
            ))
            .await
            .unwrap();
        let reconnected = tokio::time::timeout(Duration::from_secs(5), async {
            let mut output = Vec::new();
            while let Some(message) = takeover.next().await {
                if let Message::Binary(bytes) = message.unwrap() {
                    output.extend_from_slice(&bytes);
                    if output
                        .windows(b"VVWS_RECONNECTED".len())
                        .any(|window| window == b"VVWS_RECONNECTED")
                    {
                        return true;
                    }
                }
            }
            false
        })
        .await
        .unwrap();
        assert!(
            reconnected,
            "reattachment did not preserve the shell session"
        );

        takeover
            .send(Message::Binary(
                b"i=0; while :; do echo $i; i=$((i+1)); done\r"
                    .to_vec()
                    .into(),
            ))
            .await
            .unwrap();
        let (mut recovery, _) =
            tokio_tungstenite::connect_async(websocket_request(&url, "http://127.0.0.1:3000"))
                .await
                .unwrap();
        recovery
            .send(Message::Text(
                serde_json::json!({"type": "hello", "protocol": 1, "token": token})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        assert!(matches!(
            recovery.next().await.unwrap().unwrap(),
            Message::Text(ref text) if text.contains("hello")
        ));
        let recovered = tokio::time::timeout(Duration::from_secs(10), async {
            for request_id in 100..300 {
                recovery.send(attach(request_id, false)).await.unwrap();
                match recovery.next().await.unwrap().unwrap() {
                    Message::Text(text) if text.contains("session_occupied") => {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                    Message::Text(text) if text.contains(r#""type":"attached""#) => return true,
                    other => panic!("unexpected recovery response: {other:?}"),
                }
            }
            false
        })
        .await
        .unwrap();
        assert!(
            recovered,
            "a stalled WebSocket kept the session attachment occupied"
        );
        let recovery_init = tokio::time::timeout(Duration::from_secs(3), recovery.next())
            .await
            .expect("recovery terminal initialization timed out")
            .expect("recovery connection closed")
            .unwrap();
        assert!(matches!(recovery_init, Message::Binary(_)));
        recovery
            .send(Message::Binary(vec![0x03].into()))
            .await
            .unwrap();

        let stalled_closed = tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(message) = takeover.next().await {
                match message {
                    Ok(Message::Close(frame)) => {
                        return frame.is_some_and(|frame| u16::from(frame.code) == 1013);
                    }
                    Err(_) => return true,
                    _ => {}
                }
                tokio::task::yield_now().await;
            }
            true
        })
        .await
        .unwrap();
        assert!(stalled_closed, "stalled WebSocket was not disconnected");

        recovery
            .send(Message::Text(r#"{"type":"detach"}"#.into()))
            .await
            .unwrap();
        // The session actor takes terminal output and client control on one ordered event
        // channel, so a detach is serviced only after the output already queued ahead of it.
        // This pane just ran an unthrottled echo loop, and draining that backlog measured
        // ~3.5 s here even though only ~5 KB reached this socket: the wait is the actor
        // parsing and rendering pending output, not transfer. The attached terminal client
        // accommodates the same latency with its own DETACH_ACK_TIMEOUT. Bound this well
        // above the observed cost so it still catches a detach that never lands.
        let recovery_detached = tokio::time::timeout(Duration::from_secs(20), async {
            while let Some(message) = recovery.next().await {
                if matches!(message.unwrap(), Message::Text(ref text) if text.contains("detached"))
                {
                    return true;
                }
            }
            false
        })
        .await
        .unwrap();
        assert!(recovery_detached);

        let killed = command(executable, &runtime_root, &config_root)
            .args(["kill-session", "-t", &session])
            .output()
            .unwrap();
        assert!(killed.status.success());
    });
}
