//! End-to-end tests for `vvmux cloud enroll` and `vvmux serve --connect`
//! (VVTUN/1), against an in-process harness server.
//!
//! These exercise the real binary: enrollment over plain HTTP, the control
//! tunnel handshake with the Ed25519 signature, machine_status, leg dialing, a
//! real VVWS session through a tunnel leg, ticket rejection, tunnel loss, and
//! session survival.

mod common;

use std::time::Duration;

use common::{ClientControl, TunnelHarness};
use futures_util::{SinkExt, StreamExt};
use tokio::time::timeout;

fn private_directory(path: &std::path::Path) {
    std::fs::create_dir_all(path).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
}

/// Enroll a machine and start its gateway in connect mode.
async fn enroll_and_connect(
    harness: &TunnelHarness,
    runtime: &std::path::Path,
) -> (String, std::process::Child) {
    let identity = runtime.join("cloud-identity.json");
    let enroll = common::vvmux_command(runtime)
        .args(["cloud", "enroll", "test-code"])
        .arg("--server")
        .arg(harness.enroll_url())
        .arg("--identity-file")
        .arg(&identity)
        .output()
        .unwrap();
    assert!(
        enroll.status.success(),
        "enroll failed: {}",
        String::from_utf8_lossy(&enroll.stderr)
    );
    let machine_id = {
        let output = String::from_utf8_lossy(&enroll.stdout);
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

    let gateway = common::vvmux_command(runtime)
        .args(["serve", "--connect"])
        .arg(harness.url())
        .arg("--identity-file")
        .arg(&identity)
        .args(["--tunnel-heartbeat-ms", "250"])
        .args(["--tunnel-miss-limit", "3"])
        .spawn()
        .unwrap();
    (machine_id, gateway)
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
    std::fs::remove_dir_all(&runtime).ok();
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
