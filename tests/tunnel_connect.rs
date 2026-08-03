//! End-to-end tests for `vvmux cloud enroll` and `vvmux serve --connect`
//! (VVTUN/1), against an in-process harness server.
//!
//! These exercise the real binary: enrollment over plain HTTP, the control
//! tunnel handshake with the Ed25519 signature, machine_status, leg dialing, a
//! real VVWS session through a tunnel leg, ticket rejection, tunnel loss, and
//! session survival.

mod common;

use std::io::Write as _;
use std::process::Stdio;
use std::time::Duration;

use base64::Engine as _;
use common::{ClientControl, LegSocket, TunnelHarness};
use ed25519_dalek::Signature;
use futures_util::{SinkExt, StreamExt};
use sha2::{Digest as _, Sha256};
use tokio::time::timeout;

fn private_directory(path: &std::path::Path) {
    std::fs::create_dir_all(path).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
}

fn enroll_machine(
    harness: &TunnelHarness,
    runtime: &std::path::Path,
    identity: &std::path::Path,
    code: &str,
) -> std::process::Output {
    let mut child = common::vvmux_command(runtime)
        .args(["cloud", "enroll"])
        .arg("--server")
        .arg(harness.enroll_url())
        .args(["--code-file", "-"])
        .arg("--identity-file")
        .arg(identity)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    // The code is supplied only after process creation, and never through argv
    // or the environment. Linux lets this integration test verify the actual
    // kernel-visible process records rather than merely inspecting Command.
    #[cfg(target_os = "linux")]
    {
        let cmdline = std::fs::read(format!("/proc/{}/cmdline", child.id())).unwrap();
        let environ = std::fs::read(format!("/proc/{}/environ", child.id())).unwrap();
        assert!(
            !cmdline
                .windows(code.len())
                .any(|window| window == code.as_bytes())
        );
        assert!(
            !environ
                .windows(code.len())
                .any(|window| window == code.as_bytes())
        );
    }

    let mut stdin = child.stdin.take().unwrap();
    writeln!(stdin, "{code}").unwrap();
    drop(stdin);
    child.wait_with_output().unwrap()
}

fn enroll_identity(
    harness: &TunnelHarness,
    runtime: &std::path::Path,
) -> (String, std::path::PathBuf) {
    let identity = runtime.join("cloud-identity.json");
    let enroll = enroll_machine(harness, runtime, &identity, "test-code");
    assert!(
        enroll.status.success(),
        "enroll failed: {}",
        String::from_utf8_lossy(&enroll.stderr)
    );
    let machine_id = {
        let output = String::from_utf8_lossy(&enroll.stdout);
        assert!(
            output.contains(&format!(
                "vvmux serve --connect {} --acknowledge-content-visible-gateway",
                harness.enroll_url()
            )),
            "enrollment printed an unusable connect command: {output}"
        );
        output
            .lines()
            .next()
            .and_then(|line| line.strip_prefix("enrolled machine "))
            .and_then(|line| line.split_whitespace().next())
            .expect("enroll printed the machine id")
            .to_owned()
    };
    assert!(
        harness.registered_key(&machine_id).is_some(),
        "harness did not record the enrolled key"
    );
    (machine_id, identity)
}

fn start_gateway(
    harness: &TunnelHarness,
    runtime: &std::path::Path,
    identity: &std::path::Path,
) -> std::process::Child {
    common::vvmux_command(runtime)
        .args(["serve", "--connect"])
        .arg(harness.enroll_url())
        .arg("--identity-file")
        .arg(identity)
        .args(["--tunnel-heartbeat-ms", "250"])
        .args(["--tunnel-miss-limit", "3"])
        .args(["--tunnel-handshake-timeout-ms", "250"])
        .spawn()
        .unwrap()
}

/// Enroll a machine and start its gateway in connect mode.
async fn enroll_and_connect(
    harness: &TunnelHarness,
    runtime: &std::path::Path,
) -> (String, std::process::Child) {
    let (machine_id, identity) = enroll_identity(harness, runtime);
    let gateway = start_gateway(harness, runtime, &identity);
    (machine_id, gateway)
}

async fn complete_leg_hello(leg: &mut LegSocket) {
    leg.sink
        .send(axum::extract::ws::Message::Text(
            r#"{"type":"hello","protocol":1,"auth":"tunnel"}"#.into(),
        ))
        .await
        .unwrap();
    let message = timeout(Duration::from_secs(10), leg.stream.next())
        .await
        .expect("no hello reply")
        .expect("leg closed during hello")
        .expect("leg hello failed");
    let axum::extract::ws::Message::Text(text) = message else {
        panic!("expected text hello reply");
    };
    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(value["type"], "hello");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn enroll_then_tunnel_authenticates_and_reports_status() {
    let runtime = std::env::temp_dir().join(format!("vvmux-tun-auth-{}", std::process::id()));
    private_directory(&runtime);
    let harness = TunnelHarness::start("test-code").await;
    let (_machine_id, mut gateway) = enroll_and_connect(&harness, &runtime).await;

    let status = timeout(Duration::from_secs(10), harness.next_control())
        .await
        .expect("no machine_status")
        .expect("tunnel closed before machine_status");
    let ClientControl::MachineStatus {
        vvmux_version,
        vvws_protocol,
        capabilities,
    } = status
    else {
        panic!("expected machine_status, got {status:?}");
    };
    assert_eq!(vvws_protocol, 1);
    assert!(!vvmux_version.is_empty());
    assert!(
        capabilities.iter().any(|cap| cap == "tunnel-attached-v1"),
        "tunnel capabilities were {capabilities:?}"
    );
    assert!(!capabilities.iter().any(|cap| cap == "session-kill-v1"));

    gateway.kill().unwrap();
    gateway.wait().unwrap();
    std::fs::remove_dir_all(&runtime).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn enrollment_preflights_identity_and_uses_the_correct_http_authority() {
    let runtime =
        std::env::temp_dir().join(format!("vvmux-tun-enroll-preflight-{}", std::process::id()));
    private_directory(&runtime);
    let harness = TunnelHarness::start("test-code").await;
    let identity = runtime.join("cloud-identity.json");
    std::fs::write(&identity, "existing").unwrap();
    let code_file = runtime.join("enrollment-code");
    std::fs::write(&code_file, "test-code\n").unwrap();

    let refused = common::vvmux_command(&runtime)
        .args(["cloud", "enroll"])
        .arg("--server")
        .arg(harness.enroll_url())
        .arg("--code-file")
        .arg(&code_file)
        .arg("--identity-file")
        .arg(&identity)
        .output()
        .unwrap();
    assert!(!refused.status.success());
    assert!(
        harness.last_enrollment_host().is_none(),
        "existing identity still reached enrollment server"
    );

    std::fs::remove_file(&identity).unwrap();
    let enrolled = enroll_machine(&harness, &runtime, &identity, "test-code");
    assert!(
        enrolled.status.success(),
        "one-use code was consumed by preflight failure"
    );
    let expected_host = format!("127.0.0.1:{}", harness.addr.port());
    assert_eq!(
        harness.last_enrollment_host().as_deref(),
        Some(expected_host.as_str())
    );

    std::fs::remove_dir_all(&runtime).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn enrollment_rejects_a_machine_id_for_another_key_and_removes_reservation() {
    let runtime = std::env::temp_dir().join(format!("vvmux-tun-enroll-id-{}", std::process::id()));
    private_directory(&runtime);
    let harness = TunnelHarness::start("test-code").await;
    harness.set_enrollment_machine_id_override("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA");
    let identity = runtime.join("cloud-identity.json");
    let output = enroll_machine(&harness, &runtime, &identity, "test-code");
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("different public key"),
        "stderr was {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !identity.exists(),
        "failed enrollment left a reserved identity file"
    );
    std::fs::remove_dir_all(&runtime).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_initial_challenge_timeout_reconnects() {
    let runtime = std::env::temp_dir().join(format!(
        "vvmux-tun-challenge-timeout-{}",
        std::process::id()
    ));
    private_directory(&runtime);
    let harness = TunnelHarness::start("test-code").await;
    harness.set_challenge_enabled(false);
    let (_machine_id, identity) = enroll_identity(&harness, &runtime);
    let mut gateway = start_gateway(&harness, &runtime, &identity);

    timeout(Duration::from_secs(4), async {
        while harness.control_attempts() < 2 {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("gateway did not reconnect after the initial challenge timeout");

    gateway.kill().unwrap();
    gateway.wait().unwrap();
    std::fs::remove_dir_all(&runtime).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rejected_upgrade_honors_retry_after() {
    let runtime =
        std::env::temp_dir().join(format!("vvmux-tun-retry-after-{}", std::process::id()));
    private_directory(&runtime);
    let harness = TunnelHarness::start("test-code").await;
    let (_machine_id, identity) = enroll_identity(&harness, &runtime);
    harness.reject_next_control(1);
    let mut gateway = start_gateway(&harness, &runtime, &identity);

    let status = timeout(Duration::from_secs(5), harness.next_control())
        .await
        .expect("gateway did not reconnect after Retry-After")
        .expect("control tunnel closed");
    assert!(matches!(status, ClientControl::MachineStatus { .. }));
    let attempts = harness.control_attempt_times();
    assert!(attempts.len() >= 2);
    assert!(
        attempts[1].duration_since(attempts[0]) >= Duration::from_millis(900),
        "Retry-After was not honored: {:?}",
        attempts[1].duration_since(attempts[0])
    );

    gateway.kill().unwrap();
    gateway.wait().unwrap();
    gateway.wait().unwrap();
    std::fs::remove_dir_all(&runtime).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_signature_from_the_wrong_deployment_is_rejected() {
    let runtime = std::env::temp_dir().join(format!("vvmux-tun-t5-{}", std::process::id()));
    private_directory(&runtime);
    let harness = TunnelHarness::start("test-code").await;

    // Challenge with a hostname that is not the gateway's: the gateway signs
    // the hostname it connected to, so the signature must not verify.
    harness.set_hostname("attacker.example");
    let (_machine_id, mut gateway) = enroll_and_connect(&harness, &runtime).await;

    // The harness rejects the auth; the gateway reconnects. The harness keeps
    // challenging with the wrong hostname, so no machine_status ever arrives.
    let status = timeout(Duration::from_secs(3), harness.next_control()).await;
    assert!(
        status.is_err(),
        "a wrong-hostname signature must never authenticate"
    );

    gateway.kill().unwrap();
    std::fs::remove_dir_all(&runtime).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tunnel_leg_drives_a_real_session() {
    let runtime = std::env::temp_dir().join(format!("vvmux-tun-leg-{}", std::process::id()));
    private_directory(&runtime);
    let harness = TunnelHarness::start("test-code").await;

    // Create a detached session in the same runtime as the gateway.
    let created = common::vvmux_command(&runtime)
        .args(["new", "-s", "tun", "-d"])
        .output()
        .unwrap();
    assert!(created.status.success(), "session creation failed");

    let (_machine_id, mut gateway) = enroll_and_connect(&harness, &runtime).await;
    let status = timeout(Duration::from_secs(10), harness.next_control())
        .await
        .expect("no machine_status")
        .expect("tunnel closed");
    assert!(matches!(status, ClientControl::MachineStatus { .. }));

    // Open a VVWS leg.
    harness.open_leg(1, "vvws", Vec::new()).await;
    let mut leg = harness.accept_leg(1).await;

    // The tunnel hello form, then attach to the real session.
    leg.sink
        .send(axum::extract::ws::Message::Text(
            r#"{"type":"hello","protocol":1,"auth":"tunnel"}"#.into(),
        ))
        .await
        .unwrap();

    let hello = timeout(Duration::from_secs(10), leg.stream.next())
        .await
        .expect("no hello reply")
        .expect("leg closed")
        .expect("leg error");
    let axum::extract::ws::Message::Text(hello) = hello else {
        panic!("expected a text hello reply");
    };
    let hello: serde_json::Value = serde_json::from_str(&hello).unwrap();
    assert_eq!(hello["type"], "hello");
    assert!(
        hello["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|cap| cap == "tunnel-attached-v1")
    );
    assert!(hello["vivid"]["wire_version"] == "1.5");

    leg.sink
        .send(axum::extract::ws::Message::Text(
            serde_json::json!({
                "type": "attach", "request_id": 1, "name": "tun",
                "display": {"columns": 80, "rows": 24, "cell_width": 9, "cell_height": 18},
                "takeover": true, "vivid": false
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();

    let mut attached = false;
    let mut got_render = false;
    let deadline = tokio::time::sleep(Duration::from_secs(10));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => panic!("no attached/render over the tunnel leg"),
            message = leg.stream.next() => {
                let Some(Ok(message)) = message else { panic!("leg closed early") };
                match message {
                    axum::extract::ws::Message::Text(text) => {
                        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
                        assert_ne!(value["type"], "error", "attach error: {value}");
                        if value["type"] == "attached" {
                            assert_eq!(value["name"], "tun");
                            attached = true;
                        }
                    }
                    axum::extract::ws::Message::Binary(_) => got_render = true,
                    _ => {}
                }
                if attached && got_render {
                    break;
                }
            }
        }
    }
    assert!(attached, "never attached");

    // Terminal input through the leg reaches a real shell.
    leg.sink
        .send(axum::extract::ws::Message::Binary(
            "printf 'tunnel-ok\\n'\r".as_bytes().to_vec().into(),
        ))
        .await
        .unwrap();
    let echoed = timeout(Duration::from_secs(10), async {
        loop {
            match leg.stream.next().await {
                Some(Ok(axum::extract::ws::Message::Binary(bytes))) => {
                    if bytes.windows(b"tunnel-ok".len()).any(|w| w == b"tunnel-ok") {
                        return true;
                    }
                }
                Some(Ok(_)) => {}
                _ => return false,
            }
        }
    })
    .await
    .expect("no echo through the leg");
    assert!(echoed, "terminal input did not reach the shell");

    // Clean detach.
    leg.sink
        .send(axum::extract::ws::Message::Text(
            r#"{"type":"detach"}"#.into(),
        ))
        .await
        .unwrap();
    let detached = timeout(Duration::from_secs(10), async {
        loop {
            match leg.stream.next().await {
                Some(Ok(axum::extract::ws::Message::Text(text))) => {
                    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
                    if value["type"] == "detached" {
                        return true;
                    }
                }
                Some(Ok(_)) => {}
                _ => return false,
            }
        }
    })
    .await
    .expect("no detached");
    assert!(detached, "never detached");

    gateway.kill().unwrap();
    gateway.wait().unwrap();
    std::fs::remove_dir_all(&runtime).ok();
}

async fn wt_send_json(send: &mut wtransport::SendStream, value: serde_json::Value) {
    let bytes = serde_json::to_vec(&value).unwrap();
    send.write_all(&(bytes.len() as u32).to_be_bytes())
        .await
        .unwrap();
    send.write_all(&bytes).await.unwrap();
}

async fn wt_read_json(recv: &mut wtransport::RecvStream) -> serde_json::Value {
    let mut length = [0_u8; 4];
    recv.read_exact(&mut length).await.unwrap();
    let mut bytes = vec![0_u8; u32::from_be_bytes(length) as usize];
    recv.read_exact(&mut bytes).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

async fn wt_send_frame(send: &mut wtransport::SendStream, kind: u8, bytes: &[u8]) {
    let mut header = [0_u8; 5];
    header[0] = kind;
    header[1..].copy_from_slice(&(bytes.len() as u32).to_be_bytes());
    send.write_all(&header).await.unwrap();
    send.write_all(bytes).await.unwrap();
}

async fn wt_read_frame(recv: &mut wtransport::RecvStream) -> (u8, Vec<u8>) {
    let mut header = [0_u8; 5];
    recv.read_exact(&mut header).await.unwrap();
    let mut bytes = vec![0_u8; u32::from_be_bytes(header[1..].try_into().unwrap()) as usize];
    recv.read_exact(&mut bytes).await.unwrap();
    (header[0], bytes)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_webtransport_stream_drives_a_real_session_without_another_machine_connection() {
    let runtime = std::env::temp_dir().join(format!("vvmux-tun-wt-{}", std::process::id()));
    private_directory(&runtime);
    let enrollment = TunnelHarness::start("test-code").await;
    let (machine_id, identity_file) = enroll_identity(&enrollment, &runtime);
    let public_key = enrollment.registered_key(&machine_id).unwrap();
    let created = common::vvmux_command(&runtime)
        .args(["new", "-s", "wt", "-d"])
        .output()
        .unwrap();
    assert!(created.status.success());

    let probe = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let address = probe.local_addr().unwrap();
    drop(probe);
    let identity = wtransport::Identity::self_signed(["127.0.0.1"]).unwrap();
    let certificate_hash = hex::encode(Sha256::digest(
        identity.certificate_chain().as_slice()[0].der(),
    ));
    let config = wtransport::ServerConfig::builder()
        .with_bind_address(address)
        .with_identity(identity)
        .keep_alive_interval(Some(Duration::from_millis(100)))
        .build();
    let endpoint = wtransport::Endpoint::server(config).unwrap();
    let (ready_sender, ready_receiver) = tokio::sync::oneshot::channel();
    let (open_sender, open_receiver) = tokio::sync::oneshot::channel();
    let (leg_sender, leg_receiver) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let request = endpoint.accept().await.await.unwrap();
        assert_eq!(request.path(), "/t/v1/webtransport");
        let connection = request
            .accept_with_headers([("Sec-WebTransport-Protocol", "vvtun.v1")])
            .await
            .unwrap();
        let mut exporter = [0_u8; 32];
        connection
            .export_keying_material(&mut exporter, b"EXPORTER-VVTUN-1", &[])
            .unwrap();
        let (mut control_send, mut control_recv) =
            connection.open_bi().await.unwrap().await.unwrap();
        let nonce = [0x5a_u8; 32];
        wt_send_json(
            &mut control_send,
            serde_json::json!({
                "type":"challenge", "protocol":1,
                "nonce":base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(nonce),
                "hostname":"127.0.0.1"
            }),
        )
        .await;
        let auth = wt_read_json(&mut control_recv).await;
        assert_eq!(auth["machine_id"], machine_id);
        let signature = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(auth["signature"].as_str().unwrap())
            .unwrap();
        let signature = Signature::from_slice(&signature).unwrap();
        let mut signed = Vec::new();
        signed.extend_from_slice(common::AUTH_DOMAIN);
        signed.extend_from_slice(&nonce);
        signed.extend_from_slice(b"127.0.0.1");
        signed.extend_from_slice(&exporter);
        signed.extend_from_slice(machine_id.as_bytes());
        public_key.verify_strict(&signed, &signature).unwrap();
        wt_send_json(
            &mut control_send,
            serde_json::json!({
                "type":"authed", "protocol":1, "server_version":"test-wt",
                "reconnect_after_seconds":0
            }),
        )
        .await;
        let status = wt_read_json(&mut control_recv).await;
        assert_eq!(status["type"], "machine_status");
        assert!(
            status["capabilities"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v == "webtransport-streams-v1")
        );
        ready_sender.send(()).unwrap();
        open_receiver.await.unwrap();
        let ticket = [0x6b_u8; 32];
        wt_send_json(
            &mut control_send,
            serde_json::json!({
                "type":"open_leg", "leg_id":1, "kind":"vvws", "route":"test-route",
                "account":"issuer#subject",
                "ticket":base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(ticket),
                "subprotocols":[]
            }),
        )
        .await;
        let (leg_send, leg_recv) = connection.open_bi().await.unwrap().await.unwrap();
        let mut preface = [0_u8; 64];
        preface[..8].copy_from_slice(b"VVTLEG1\0");
        preface[8..16].copy_from_slice(&17_u64.to_be_bytes());
        preface[16..24].copy_from_slice(&1_u64.to_be_bytes());
        preface[24] = 1;
        preface[25..29].copy_from_slice(&100_i32.to_be_bytes());
        preface[32..].copy_from_slice(&ticket);
        let mut leg_send = leg_send;
        leg_send.write_all(&preface).await.unwrap();
        leg_sender.send((leg_send, leg_recv)).unwrap();
        connection.closed().await;
    });

    let mut gateway = common::vvmux_command(&runtime)
        .args(["serve", "--connect"])
        .arg(format!("https://{address}"))
        .arg("--identity-file")
        .arg(&identity_file)
        .args(["--tunnel-carrier", "webtransport"])
        .arg("--tunnel-certificate-sha256")
        .arg(&certificate_hash)
        .arg("--acknowledge-content-visible-gateway")
        .spawn()
        .unwrap();
    timeout(Duration::from_secs(10), ready_receiver)
        .await
        .expect("gateway did not authenticate WebTransport")
        .unwrap();
    open_sender.send(()).unwrap();
    let (mut leg_send, mut leg_recv) = timeout(Duration::from_secs(10), leg_receiver)
        .await
        .expect("gateway did not accept WebTransport leg")
        .unwrap();

    wt_send_frame(
        &mut leg_send,
        1,
        br#"{"type":"hello","protocol":1,"auth":"tunnel"}"#,
    )
    .await;
    let (kind, hello) = timeout(Duration::from_secs(10), wt_read_frame(&mut leg_recv))
        .await
        .expect("no hello reply");
    assert_eq!(kind, 1);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&hello).unwrap()["type"],
        "hello"
    );

    wt_send_frame(
        &mut leg_send,
        1,
        serde_json::json!({
            "type":"attach", "request_id":1, "name":"wt",
            "display":{"columns":80,"rows":24,"cell_width":9,"cell_height":18},
            "takeover":true, "vivid":false
        })
        .to_string()
        .as_bytes(),
    )
    .await;
    let mut attached = false;
    let mut rendered = false;
    timeout(Duration::from_secs(10), async {
        while !(attached && rendered) {
            let (kind, bytes) = wt_read_frame(&mut leg_recv).await;
            match kind {
                1 => {
                    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                    assert_ne!(value["type"], "error", "attach error: {value}");
                    attached |= value["type"] == "attached";
                }
                2 => rendered = true,
                _ => {}
            }
        }
    })
    .await
    .expect("no attached/render over WebTransport");

    wt_send_frame(&mut leg_send, 2, b"printf 'wt-ok\\n'\r").await;
    let echoed = timeout(Duration::from_secs(10), async {
        loop {
            let (kind, bytes) = wt_read_frame(&mut leg_recv).await;
            if kind == 2 && bytes.windows(5).any(|window| window == b"wt-ok") {
                return;
            }
        }
    })
    .await;
    assert!(
        echoed.is_ok(),
        "terminal input did not reach the real session"
    );

    gateway.kill().unwrap();
    gateway.wait().unwrap();
    server.abort();
    std::fs::remove_dir_all(&runtime).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unknown_leg_ticket_is_refused_and_reported() {
    let runtime = std::env::temp_dir().join(format!("vvmux-tun-tick-{}", std::process::id()));
    private_directory(&runtime);
    let harness = TunnelHarness::start("test-code").await;
    let (_machine_id, mut gateway) = enroll_and_connect(&harness, &runtime).await;
    let status = timeout(Duration::from_secs(10), harness.next_control())
        .await
        .expect("no machine_status");
    assert!(matches!(status, Some(ClientControl::MachineStatus { .. })));

    harness.open_leg_bad_ticket(7, "vvws");
    let failed = timeout(Duration::from_secs(10), harness.next_control())
        .await
        .expect("no leg_failed")
        .expect("tunnel closed");
    let ClientControl::LegFailed { leg_id, code } = failed else {
        panic!("expected leg_failed, got {failed:?}");
    };
    assert_eq!(leg_id, 7);
    assert_eq!(code, "ticket_rejected");

    gateway.kill().unwrap();
    gateway.wait().unwrap();
    std::fs::remove_dir_all(&runtime).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn duplicate_leg_id_is_source_scoped_and_close_releases_only_its_attachment() {
    let runtime_a =
        std::env::temp_dir().join(format!("vvmux-tun-duplicate-a-{}", std::process::id()));
    let runtime_b =
        std::env::temp_dir().join(format!("vvmux-tun-duplicate-b-{}", std::process::id()));
    private_directory(&runtime_a);
    private_directory(&runtime_b);
    let harness_a = TunnelHarness::start("test-code").await;
    let harness_b = TunnelHarness::start("test-code").await;
    let (_machine_a, mut gateway_a) = enroll_and_connect(&harness_a, &runtime_a).await;
    let (_machine_b, mut gateway_b) = enroll_and_connect(&harness_b, &runtime_b).await;
    assert!(matches!(
        timeout(Duration::from_secs(10), harness_a.next_control()).await,
        Ok(Some(ClientControl::MachineStatus { .. }))
    ));
    assert!(matches!(
        timeout(Duration::from_secs(10), harness_b.next_control()).await,
        Ok(Some(ClientControl::MachineStatus { .. }))
    ));

    // Two independent tunnel generations deliberately reuse the same numeric
    // leg ID. The duplicate is injected only into owner A.
    harness_a.open_leg(9, "vvws", Vec::new()).await;
    harness_b.open_leg(9, "vvws", Vec::new()).await;
    let mut leg_a = harness_a.accept_leg(9).await;
    let mut leg_b = harness_b.accept_leg(9).await;
    harness_a.open_leg(9, "vvws", Vec::new()).await;
    let failure = timeout(Duration::from_secs(10), harness_a.next_control())
        .await
        .expect("duplicate leg was not rejected")
        .expect("owner A control tunnel closed");
    assert!(matches!(
        failure,
        ClientControl::LegFailed { leg_id: 9, ref code } if code == "invalid_request"
    ));

    // The original A leg and the same-numbered B leg both remain usable.
    complete_leg_hello(&mut leg_a).await;
    complete_leg_hello(&mut leg_b).await;

    harness_a.close_leg(9);
    timeout(Duration::from_secs(10), leg_a.stream.next())
        .await
        .expect("closing owner A's leg did not tear it down");

    // Owner B's next valid request must still work after A's scoped failure and
    // teardown.
    leg_b
        .sink
        .send(axum::extract::ws::Message::Text(
            r#"{"type":"list_sessions","request_id":41}"#.into(),
        ))
        .await
        .unwrap();
    let reply = timeout(Duration::from_secs(10), leg_b.stream.next())
        .await
        .expect("owner B stopped responding")
        .expect("owner B leg closed")
        .expect("owner B leg failed");
    let axum::extract::ws::Message::Text(reply) = reply else {
        panic!("expected owner B sessions reply");
    };
    let reply: serde_json::Value = serde_json::from_str(&reply).unwrap();
    assert_eq!(reply["type"], "sessions");
    assert_eq!(reply["request_id"], 41);

    gateway_a.kill().unwrap();
    gateway_b.kill().unwrap();
    gateway_a.wait().unwrap();
    gateway_b.wait().unwrap();
    std::fs::remove_dir_all(&runtime_a).ok();
    std::fs::remove_dir_all(&runtime_b).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tunnel_loss_reconnects_and_sessions_survive() {
    let runtime = std::env::temp_dir().join(format!("vvmux-tun-restart-{}", std::process::id()));
    private_directory(&runtime);
    let harness = TunnelHarness::start("test-code").await;

    let created = common::vvmux_command(&runtime)
        .args(["new", "-s", "keep", "-d"])
        .output()
        .unwrap();
    assert!(created.status.success());

    let (_machine_id, mut gateway) = enroll_and_connect(&harness, &runtime).await;
    let status = timeout(Duration::from_secs(10), harness.next_control())
        .await
        .expect("no first machine_status");
    assert!(matches!(status, Some(ClientControl::MachineStatus { .. })));

    // Drop the control tunnel. The gateway must reconnect within the backoff
    // window (cap 1 s) and the session daemon must survive the whole thing.
    harness.drop_control();
    let second = timeout(Duration::from_secs(15), harness.next_control())
        .await
        .expect("no reconnect")
        .expect("tunnel never re-established");
    assert!(
        matches!(second, ClientControl::MachineStatus { .. }),
        "expected a second machine_status after reconnect"
    );

    let listed = common::vvmux_command(&runtime)
        .args(["list"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&listed.stdout);
    assert!(
        stdout.contains("keep"),
        "session did not survive the tunnel loss: {stdout}"
    );

    gateway.kill().unwrap();
    gateway.wait().unwrap();
    std::fs::remove_dir_all(&runtime).ok();
}

/// The gateway must refuse `ws://` to a non-loopback host before connecting.
#[test]
fn connect_rejects_plain_ws_across_a_host_boundary() {
    let mut command = common::vvmux_command(&std::env::temp_dir());
    command
        .args(["serve", "--connect"])
        .arg("ws://vvmux.example/t/v1/control");
    let output = command.output().unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("loopback development only"),
        "stderr was: {stderr}"
    );
}

#[test]
fn public_connect_requires_content_visibility_acknowledgement() {
    let runtime = std::env::temp_dir().join(format!("vvmux-tun-ack-{}", std::process::id()));
    private_directory(&runtime);
    let output = common::vvmux_command(&runtime)
        .args(["serve", "--connect", "https://vvmux.example"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--acknowledge-content-visible-gateway"),
        "stderr was: {stderr}"
    );
    std::fs::remove_dir_all(&runtime).ok();
}
