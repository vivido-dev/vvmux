use super::*;

fn overlay_window(
    x: i64,
    y: i64,
    width: i64,
    height: i64,
) -> vivid_sdk::presenter::SnapshotOverlayWindow {
    vivid_sdk::presenter::SnapshotOverlayWindow {
        parent: None,
        min_width: 1,
        min_height: 1,
        generation: 1,
        revision: 1,
        x,
        y,
        width,
        height,
        mode: 0,
        visible: true,
    }
}

fn metrics() -> DisplayMetrics {
    DisplayMetrics {
        columns: 100,
        rows: 40,
        cell_width: 8,
        cell_height: 16,
    }
}

#[test]
fn an_overlay_window_is_placed_at_its_pane_origin_in_the_outer_terminal() {
    // A pane starting at cell (10, 4) with 8x16 cells begins at pixel (80, 64), so a window
    // the producer put at pane-local (12, 20) belongs at (92, 84) on the outer terminal.
    let pane = Rect {
        x: 10,
        y: 4,
        width: 40,
        height: 20,
    };
    let projected = project_overlay_window(overlay_window(12, 20, 240, 120), pane, metrics());
    assert_eq!((projected.x, projected.y), (92, 84));
    assert_eq!((projected.width, projected.height), (240, 120));
    assert!(projected.visible);
}

#[test]
fn an_overlay_window_is_pulled_back_inside_its_pane_rather_than_over_a_neighbour() {
    // The pane is 320x320 pixels and the window is 240x120, so its origin can reach 80x200.
    let pane = Rect {
        x: 0,
        y: 0,
        width: 40,
        height: 20,
    };
    let projected = project_overlay_window(overlay_window(300, 400, 240, 120), pane, metrics());
    assert_eq!(
        (projected.x, projected.y),
        (80, 200),
        "an overhanging window is moved, never resized: its producer laid out that size"
    );
    assert!(projected.visible);
}

#[test]
fn an_overlay_window_too_large_for_its_pane_is_withheld() {
    // `SET_OVERLAY_WINDOW` has no clip, so the only alternative to withholding it is letting
    // it paint across the panes beside it.
    let pane = Rect {
        x: 0,
        y: 0,
        width: 10,
        height: 5,
    };
    let projected = project_overlay_window(overlay_window(0, 0, 240, 120), pane, metrics());
    assert!(!projected.visible);
}

#[test]
fn a_window_its_producer_hid_stays_hidden_wherever_it_would_land() {
    let pane = Rect {
        x: 2,
        y: 2,
        width: 40,
        height: 20,
    };
    let mut hidden = overlay_window(0, 0, 100, 50);
    hidden.visible = false;
    assert!(!project_overlay_window(hidden, pane, metrics()).visible);
}

#[test]
fn a_pointer_reaches_an_overlay_in_the_pane_local_pixels_it_laid_out_against() {
    let pane = Rect {
        x: 10,
        y: 4,
        width: 40,
        height: 20,
    };
    let mouse = MouseEvent {
        button: 0,
        x: 12,
        y: 6,
        kind: MouseKind::Press,
        shift: false,
        alt: false,
        ctrl: false,
    };
    // The outer terminal reported an exact pixel inside that cell.
    assert_eq!(
        overlay_pointer_position(mouse, Some((100, 70)), pane, metrics()),
        (20., 6.)
    );
    // Without pixel reporting the position is the middle of the cell, so a click still lands
    // on whatever occupies it.
    assert_eq!(
        overlay_pointer_position(mouse, None, pane, metrics()),
        (20., 40.)
    );
}

#[test]
fn a_pane_publishes_its_frame_and_pane_as_a_mesh_address() {
    assert_eq!(mesh_address(1, 2).as_deref(), Some("f1p2"));
    assert_eq!(mesh_address(3, 41).as_deref(), Some("f3p41"));
}

#[test]
fn an_id_that_cannot_be_an_address_index_publishes_no_address() {
    // An address index is a one-based u32. Publishing one that cannot parse fails `vvagent
    // bind` outright and takes the whole mailbox with it; publishing none costs the position
    // and nothing else.
    assert_eq!(mesh_address(0, 2), None);
    assert_eq!(mesh_address(1, 0), None);
    assert_eq!(mesh_address(u64::from(u32::MAX) + 1, 2), None);
    assert_eq!(mesh_address(1, u64::from(u32::MAX) + 1), None);
    assert_eq!(
        mesh_address(u64::from(u32::MAX), 1).as_deref(),
        Some("f4294967295p1")
    );
}

fn delayed(due: Instant, sequence: u64) -> DelayedInput {
    DelayedInput {
        due,
        sequence,
        pane_id: 1,
        bytes: vec![b'\r'],
    }
}

#[test]
fn a_scaled_capture_waits_out_the_blank_frame_of_a_replacement_track() {
    // The bug this locks: a producer answers the resize by replacing its raster track, and
    // that track's first frame is the blank buffer it was created with. Resolving on the first
    // change wrote an empty page to disk while reporting a plausible size and frame id.
    let before = vec![(2_u64, 1485_u32, 1666_u32, Some(2_u64))];
    let blank = vec![(2, 2970, 3332, Some(1))];
    let rendered = vec![(2, 2970, 3332, Some(3))];
    let start = Instant::now();

    // Nothing has reached the producer yet.
    assert_eq!(
        capture_settle_step(&before, before.clone(), None, start),
        CaptureSettleStep::Wait(None)
    );

    // The replacement track appears. This must not capture.
    let CaptureSettleStep::Wait(Some(first)) =
        capture_settle_step(&before, blank.clone(), None, start)
    else {
        panic!("the blank first frame of a replacement track was captured");
    };
    assert_eq!(first.seen, blank);

    // Still inside the quiet window with nothing new: keep waiting rather than capture.
    assert_eq!(
        capture_settle_step(&before, blank.clone(), Some(&first), start),
        CaptureSettleStep::Wait(None)
    );

    // The render lands, which restarts the quiet window.
    let CaptureSettleStep::Wait(Some(second)) = capture_settle_step(
        &before,
        rendered.clone(),
        Some(&first),
        start + Duration::from_millis(100),
    ) else {
        panic!("a newer frame must restart the settle rather than resolve it");
    };
    assert_eq!(second.seen, rendered);
    assert_eq!(
        second.give_up_at, first.give_up_at,
        "the bound is not reset"
    );

    // Quiet elapses with nothing newer, so the rendered frame is the one captured.
    assert_eq!(
        capture_settle_step(
            &before,
            rendered.clone(),
            Some(&second),
            second.quiet_since + CAPTURE_SETTLE_QUIET,
        ),
        CaptureSettleStep::Capture
    );
}

#[test]
fn a_pane_that_never_goes_quiet_still_answers() {
    // A page that animates forever would never satisfy the quiet window. The give-up bound
    // makes the capture terminate with a real frame instead of a timeout.
    let before = vec![(2_u64, 100_u32, 100_u32, Some(1_u64))];
    let start = Instant::now();
    let settling = CaptureSettling {
        seen: vec![(2, 200, 200, Some(9))],
        quiet_since: start,
        give_up_at: start + CAPTURE_SETTLE_LIMIT,
    };
    // Frames still arriving right at the bound: capture anyway.
    assert_eq!(
        capture_settle_step(
            &before,
            settling.seen.clone(),
            Some(&settling),
            start + CAPTURE_SETTLE_LIMIT,
        ),
        CaptureSettleStep::Capture
    );
    assert!(CAPTURE_SETTLE_QUIET < CAPTURE_SETTLE_LIMIT);
}

fn changed(sequence: u64, rows: Option<Vec<usize>>, at: Instant) -> ScreenChange {
    ScreenChange { sequence, rows, at }
}

#[test]
fn a_ticking_status_bar_does_not_count_as_pane_activity() {
    // The failure this fixes, measured on a real agent pane: a TUI repainting a seconds
    // counter into its status bar changes the screen every second, so an unqualified
    // stability wait can never fire while the agent is working — which is precisely when a
    // caller is waiting for it to finish.
    let base = Instant::now();
    let last = base + Duration::from_secs(10);
    let changes = VecDeque::from([
        // Real output, five seconds ago.
        changed(1, Some(vec![3, 4]), base + Duration::from_secs(5)),
        // Status-bar repaints since then, in the bottom four rows.
        changed(2, Some(vec![21]), base + Duration::from_secs(8)),
        changed(3, Some(vec![22, 23]), base + Duration::from_secs(10)),
    ]);

    // Counting every row, the pane looks busy as of the last tick.
    assert_eq!(
        last_meaningful_change(&changes, last, 24, 0),
        base + Duration::from_secs(10)
    );
    // Discounting the status bar, it has been quiet since the real output.
    assert_eq!(
        last_meaningful_change(&changes, last, 24, 4),
        base + Duration::from_secs(5)
    );
}

#[test]
fn a_whole_screen_repaint_is_always_activity() {
    // `rows: None` is a screen switch or clear. Nothing says it was confined to the discounted
    // band, so it must never be dismissed as status-bar noise.
    let base = Instant::now();
    let last = base + Duration::from_secs(9);
    let changes = VecDeque::from([
        changed(1, Some(vec![2]), base + Duration::from_secs(1)),
        changed(2, None, base + Duration::from_secs(9)),
    ]);
    assert_eq!(
        last_meaningful_change(&changes, last, 24, 4),
        base + Duration::from_secs(9)
    );
}

#[test]
fn a_pane_that_only_ever_ticked_reports_its_oldest_retained_change() {
    // With nothing but status-bar noise retained, the honest answer is "quiet since at least
    // the oldest thing still remembered" rather than "never changed".
    let base = Instant::now();
    let last = base + Duration::from_secs(9);
    let changes = VecDeque::from([
        changed(1, Some(vec![22]), base + Duration::from_secs(3)),
        changed(2, Some(vec![23]), base + Duration::from_secs(9)),
    ]);
    assert_eq!(
        last_meaningful_change(&changes, last, 24, 4),
        base + Duration::from_secs(3)
    );
    // Discounting every row cannot make a pane look busy either.
    assert_eq!(
        last_meaningful_change(&changes, last, 24, 99),
        base + Duration::from_secs(3)
    );
}

#[test]
fn a_capture_scale_multiplies_the_advertised_cell_size() {
    // The producer sizes its raster from the cell metrics, so this is the only lever the
    // closed terminal-surface-v1 descriptor offers for asking it to re-render denser.
    assert_eq!(scaled_cells(3, 9, None), (3, 9));
    assert_eq!(scaled_cells(3, 9, Some(0)), (3, 9));
    assert_eq!(scaled_cells(3, 9, Some(1)), (3, 9));
    assert_eq!(scaled_cells(3, 9, Some(10)), (30, 90));
    // A scale that would wrap saturates instead; the caller has already bounded the request
    // against the pixel budget, so this only has to refuse to produce nonsense.
    assert_eq!(
        scaled_cells(u16::MAX, u16::MAX, Some(4)),
        (u16::MAX, u16::MAX)
    );
}

#[test]
fn a_scaled_capture_composes_at_the_larger_canvas() {
    // The bug this guards: composing against the client's cell size while the producer renders
    // at capture density resamples the extra detail straight back out, and `--scale` silently
    // does nothing. The measured pane from the design work is 101x53 cells at 3x9.
    let cells = |scale| {
        let (width, height) = scaled_cells(3, 9, scale);
        crate::capture_media::CaptureTarget {
            columns: 101,
            rows: 52,
            cell_width: u32::from(width),
            cell_height: u32::from(height),
        }
        .pixel_size()
        .expect("pane has a drawable size")
    };
    assert_eq!(cells(None), (303, 468), "unscaled, and illegible for CJK");
    assert_eq!(cells(Some(10)), (3030, 4680));
}

#[test]
fn delayed_input_drains_by_deadline_then_enqueue_order() {
    let base = Instant::now();
    let mut heap = BinaryHeap::new();
    // Pushed out of order, and with a tie on the deadline.
    heap.push(Reverse(delayed(base + Duration::from_millis(300), 2)));
    heap.push(Reverse(delayed(base + Duration::from_millis(100), 0)));
    heap.push(Reverse(delayed(base + Duration::from_millis(300), 1)));

    let order =
        std::iter::from_fn(|| heap.pop().map(|Reverse(input)| input.sequence)).collect::<Vec<_>>();
    // Earliest deadline first; equal deadlines keep the order they were queued in, which a
    // bare `Instant` key would leave to the heap to decide.
    assert_eq!(order, [0, 1, 2]);
}

#[test]
fn only_states_worth_interrupting_for_notify() {
    use crate::config::{Notifications, NotifyOn};
    use crate::ipc::NotifyKind;

    let settings = Notifications::default();
    assert_eq!(
        notification_kind(AgentStatus::Blocked, &settings),
        Some(NotifyKind::AgentBlocked)
    );
    assert_eq!(
        notification_kind(AgentStatus::Done, &settings),
        Some(NotifyKind::AgentDone)
    );
    // States an agent passes through while not asking for anything.
    assert_eq!(notification_kind(AgentStatus::Working, &settings), None);
    assert_eq!(notification_kind(AgentStatus::Idle, &settings), None);

    // `on` narrows without disabling.
    let blocked_only = Notifications {
        on: vec![NotifyOn::Blocked],
        ..Notifications::default()
    };
    assert_eq!(notification_kind(AgentStatus::Done, &blocked_only), None);
    assert_eq!(
        notification_kind(AgentStatus::Blocked, &blocked_only),
        Some(NotifyKind::AgentBlocked)
    );

    // The kill switch beats the list.
    let off = Notifications {
        enabled: false,
        ..Notifications::default()
    };
    assert_eq!(notification_kind(AgentStatus::Blocked, &off), None);
}

#[test]
fn the_notification_floor_is_per_pane_and_first_notifications_pass() {
    let now = Instant::now();
    let floor = Duration::from_millis(2_000);
    // A pane that has never notified is never throttled.
    assert!(notification_allowed(None, now, floor));
    assert!(!notification_allowed(
        Some(now.checked_sub(Duration::from_millis(500)).unwrap()),
        now,
        floor
    ));
    assert!(notification_allowed(
        Some(now.checked_sub(Duration::from_millis(2_000)).unwrap()),
        now,
        floor
    ));
    // A zero floor disables throttling rather than blocking everything.
    assert!(notification_allowed(Some(now), now, Duration::ZERO));
}

#[test]
fn notification_text_is_clamped_on_a_char_boundary() {
    let plain = bounded_notification_text("codex needs you");
    assert_eq!(plain, "codex needs you");

    // Multi-byte characters must not be split when the limit lands mid-character.
    let wide = "é".repeat(200);
    let clamped = bounded_notification_text(&wide);
    assert!(clamped.len() <= 163);
    assert!(clamped.ends_with('…'));
    assert!(wide.starts_with(clamped.trim_end_matches('…')));
}

#[test]
fn kitty_transfers_are_pane_isolated_bounded_and_drained_in_order() {
    let mut transfers = KittyTransferBuffer::default();
    assert!(transfers.push_bounded(1, b"a1".to_vec(), true, true, 12));
    assert!(transfers.push_bounded(2, b"b1".to_vec(), true, true, 12));
    assert!(transfers.push_bounded(1, b"a2".to_vec(), false, false, 12));
    assert!(transfers.push_bounded(2, b"b2".to_vec(), false, false, 12));
    assert_eq!(transfers.drain_pending(), b"a1a2b1b2");
    assert_eq!(transfers.bytes, 0);

    assert!(transfers.push_bounded(1, b"123456".to_vec(), true, true, 8));
    assert!(!transfers.push_bounded(1, b"789".to_vec(), false, false, 8));
    assert_eq!(transfers.bytes, 0);
    assert!(transfers.pending.is_empty());
    assert!(transfers.transfers.is_empty());
}

#[test]
fn clearing_kitty_transfers_drops_attachment_pixels() {
    let mut transfers = KittyTransferBuffer::default();
    assert!(transfers.push(1, b"upload".to_vec(), true, false));
    transfers.clear();
    assert_eq!(transfers.bytes, 0);
    assert!(transfers.pending.is_empty());
    assert!(transfers.transfers.is_empty());
}

#[test]
fn kitty_support_query_reports_attachment_capability() {
    assert_eq!(kitty_query_response(true, 31), b"\x1b_Gi=31;OK\x1b\\");
    assert_eq!(kitty_query_response(false, 31), b"\x1b_Gi=31;ENOTSUP\x1b\\");
}

#[test]
fn only_vetted_schemes_reach_the_host_opener() {
    assert!(is_openable_uri("https://example.test/a?b=1&c=2"));
    assert!(is_openable_uri("http://example.test/"));
    assert!(is_openable_uri("mailto:someone@example.test"));

    // An OSC 8 URI is written by whatever holds the pane, so these are the shapes an attacker
    // controls outright. None may reach a system handler.
    assert!(!is_openable_uri("file:///etc/passwd"));
    assert!(!is_openable_uri("javascript:alert(1)"));
    assert!(!is_openable_uri("smb://host/share"));
    assert!(!is_openable_uri("vscode://file/etc/passwd"));
    // A scheme with nothing after it gives the handler no target to reason about.
    assert!(!is_openable_uri("https://"));
    // Control characters must never reach an argv element.
    assert!(!is_openable_uri("https://example.test/\r\nX"));
    assert!(!is_openable_uri("https://example.test/\u{1b}]0;pwned\u{7}"));
    // Scheme matching is case-insensitive per RFC 3986.
    assert!(is_openable_uri("HTTPS://example.test/"));
    assert!(!is_openable_uri("FILE:///etc/passwd"));
}

#[test]
fn the_link_preview_keeps_the_head_of_an_over_long_uri() {
    let uri = "https://example.test/a/very/long/path/that/will/not/fit";
    let preview = hyperlink_status_text(uri, 20);
    assert_eq!(preview.chars().count(), 20);
    assert!(preview.starts_with("https://example.tes"));
    assert!(preview.ends_with('…'));

    // A URI that fits is shown whole.
    assert_eq!(
        hyperlink_status_text("https://a.test/", 40),
        "https://a.test/"
    );
    // Control characters are neutralized before the preview is composited.
    assert_eq!(
        hyperlink_status_text("https://a.test/\u{1b}x", 40),
        "https://a.test/ x"
    );
    assert_eq!(hyperlink_status_text("https://a.test/", 0), "");
}

#[cfg(windows)]
#[test]
fn windows_prefers_a_resolvable_inherited_shell_over_comspec() {
    let selected = default_windows_shell(
        Some(OsString::from("pwsh.exe")),
        Some(OsString::from(r"C:\Windows\System32\cmd.exe")),
        |shell| {
            (shell == std::ffi::OsStr::new("pwsh.exe"))
                .then(|| OsString::from(r"C:\Program Files\PowerShell\7\pwsh.exe"))
        },
    );
    assert_eq!(
        selected,
        Some(OsString::from(r"C:\Program Files\PowerShell\7\pwsh.exe"))
    );
}

#[cfg(windows)]
#[test]
fn windows_ignores_an_inherited_shell_that_is_not_a_native_executable() {
    let comspec = OsString::from(r"C:\Windows\System32\cmd.exe");
    let selected = default_windows_shell(
        Some(OsString::from("/bin/bash")),
        Some(comspec.clone()),
        |_| None,
    );
    assert_eq!(selected, Some(comspec));
}

#[test]
fn plugin_host_calls_have_explicit_capabilities_and_strict_params() {
    use vvmux_plugin_api::Permission;

    assert_eq!(
        plugin_host_permission("session.inspect"),
        Some(Permission::SessionRead)
    );
    assert_eq!(
        plugin_host_permission("pane.get_text"),
        Some(Permission::PaneRead)
    );
    assert_eq!(
        plugin_host_permission("pane.input"),
        Some(Permission::PaneInput)
    );
    assert_eq!(plugin_host_permission("pane.delete_anything"), None);
    let caller = CallerContext {
        origin: CallerOrigin::Plugin {
            plugin_id: "dev.example".into(),
            plugin_instance: "instance-a".into(),
        },
        session_instance: "session-a".into(),
        focused_fallback: false,
        capabilities: [Permission::PaneRead].into_iter().collect(),
    };
    authorize_session_scope(&caller, "session-a").unwrap();
    authorize_session_capability(&caller, Permission::PaneRead).unwrap();
    assert_eq!(
        authorize_session_capability(&caller, Permission::PaneInput)
            .unwrap_err()
            .code,
        "capability_denied"
    );
    assert_eq!(
        authorize_session_scope(&caller, "session-b")
            .unwrap_err()
            .code,
        "scope_denied"
    );
    assert_eq!(
        plugin_enforceable_capabilities(),
        [
            "session.read",
            "pane.read",
            "pane.input",
            "pane.create",
            "pane.manage_own",
            "pane.manage_any",
            "events.subscribe",
            "plugin.invoke",
            "media.produce",
        ]
    );
    require_plugin_params(&serde_json::json!({"pane_id": 1}), &["pane_id"]).unwrap();
    assert!(
        require_plugin_params(
            &serde_json::json!({"pane_id": 1, "unexpected": true}),
            &["pane_id"]
        )
        .is_err()
    );
}

#[test]
fn plugin_pane_identity_scopes_sync_management_and_generation_cleanup() {
    use vvmux_plugin_api::Permission;

    let owner = PluginPaneIdentity {
        session_instance: "session-a".into(),
        plugin_id: "dev.example".into(),
        plugin_instance: "instance-a".into(),
        package_digest: "digest-a".into(),
        entrypoint_id: "dashboard".into(),
        title: "Dashboard".into(),
        accept_sync_input: false,
    };
    let role = PaneRole::Plugin(owner.clone());
    let caller = CallerContext {
        origin: CallerOrigin::Plugin {
            plugin_id: owner.plugin_id.clone(),
            plugin_instance: owner.plugin_instance.clone(),
        },
        session_instance: owner.session_instance.clone(),
        focused_fallback: false,
        capabilities: [Permission::PaneManageOwn].into_iter().collect(),
    };
    assert!(!pane_role_accepts_sync(&role));
    let mut sync_owner = owner.clone();
    sync_owner.accept_sync_input = true;
    assert!(pane_role_accepts_sync(&PaneRole::Plugin(sync_owner)));
    assert!(caller_owns_plugin_pane(&caller, &role));
    assert!(plugin_pane_matches_generation(
        &role,
        "session-a",
        "dev.example",
        "digest-a"
    ));

    // Two owners may reuse the same numeric pane ID in separate session instances. Cleanup
    // and management decisions use the complete identity, never that local number.
    let reused_numeric_pane_id = 7_u64;
    let other_role = PaneRole::Plugin(PluginPaneIdentity {
        session_instance: "session-b".into(),
        plugin_instance: "instance-b".into(),
        ..owner.clone()
    });
    assert_eq!(reused_numeric_pane_id, 7);
    assert!(!caller_owns_plugin_pane(&caller, &other_role));
    assert!(!plugin_pane_matches_generation(
        &other_role,
        "session-a",
        "dev.example",
        "digest-a"
    ));

    let restarted = CallerContext {
        origin: CallerOrigin::Plugin {
            plugin_id: owner.plugin_id,
            plugin_instance: "instance-restarted".into(),
        },
        ..caller
    };
    assert!(!caller_owns_plugin_pane(&restarted, &role));
}

#[test]
fn agent_navigator_geometry_is_centered_and_bounded() {
    let area = Rect {
        x: 0,
        y: 0,
        width: 120,
        height: 40,
    };
    assert_eq!(
        agent_navigator_rect(area, 100),
        Some(Rect {
            x: 10,
            y: 11,
            width: 100,
            height: 18,
        })
    );
    assert!(
        agent_navigator_rect(
            Rect {
                width: 19,
                height: 10,
                ..Rect::default()
            },
            1
        )
        .is_none()
    );
}

#[test]
fn agent_navigator_keys_decode_coalesced_input_without_pty_residue() {
    let mut input = b"\x1b[A\x1b[6~j\r".as_slice();
    let mut keys = Vec::new();
    while !input.is_empty() {
        let (consumed, key) = decode_agent_navigator_key(input);
        assert!(consumed > 0);
        input = &input[consumed..];
        keys.extend(key);
    }
    assert_eq!(
        keys,
        [
            AgentNavigatorKey::Up,
            AgentNavigatorKey::PageDown,
            AgentNavigatorKey::Down,
            AgentNavigatorKey::Activate,
        ]
    );
    assert_eq!(
        decode_agent_navigator_key(b"\x1b[7~"),
        (4, Some(AgentNavigatorKey::Home))
    );
    assert_eq!(
        decode_agent_navigator_key(b"\x1b[8~"),
        (4, Some(AgentNavigatorKey::End))
    );
}

#[test]
fn prompt_input_keeps_presses_and_drops_release_and_repeat_reports() {
    // A pane running under Kitty flags 3 makes the host report key events, so the navigator
    // sees the release of the very key that opened it and the release of every key used to
    // move the selection. Each begins with ESC, which the prompt language reads as a cancel.
    assert_eq!(key_presses(b"\x1b[119;1:3u").as_ref(), b"");
    assert_eq!(key_presses(b"\x1b[1;1:2B").as_ref(), b"");
    assert_eq!(
        key_presses(b"j\x1b[106;1:3uk\x1b[107;1:3u\r").as_ref(),
        b"jk\r"
    );

    // Presses, legacy sequences, and a real Escape are the prompt's own language.
    assert_eq!(key_presses(b"\x1b[B").as_ref(), b"\x1b[B");
    assert_eq!(key_presses(b"\x1b[119u").as_ref(), b"\x1b[119u");
    assert_eq!(key_presses(b"\x1b").as_ref(), b"\x1b");
    assert!(matches!(key_presses(b"jk"), Cow::Borrowed(_)));

    let (consumed, key) = decode_agent_navigator_key(&key_presses(b"\x1b[119;1:3u"));
    assert_eq!((consumed, key), (0, None));
    assert_eq!(
        decode_agent_navigator_key(b"\x1b[119;1:3u"),
        (1, Some(AgentNavigatorKey::Close)),
        "the unfiltered report is what closed the popup"
    );
}

#[test]
fn media_wakeups_coalesce_until_the_actor_clears_pending_work() {
    let (sender, receiver) = mpsc::sync_channel(EVENT_QUEUE);
    let pending = AtomicBool::new(false);

    for _ in 0..(EVENT_QUEUE * 2) {
        request_media_service(&sender, &pending);
    }

    assert!(matches!(receiver.try_recv(), Ok(ActorEvent::MediaReady)));
    assert!(
        matches!(receiver.try_recv(), Err(mpsc::TryRecvError::Empty)),
        "one video frame must not add another redundant actor wake while media work is pending"
    );

    pending.store(false, Ordering::Release);
    request_media_service(&sender, &pending);
    assert!(matches!(receiver.try_recv(), Ok(ActorEvent::MediaReady)));
}

#[test]
fn pty_output_batching_preserves_intervening_actor_event_order() {
    let (sender, receiver) = mpsc::sync_channel(4);
    sender
        .send(ActorEvent::PtyOutput(7, b"second".to_vec()))
        .unwrap();
    sender.send(ActorEvent::MediaReady).unwrap();
    sender
        .send(ActorEvent::PtyOutput(7, b"third".to_vec()))
        .unwrap();

    let (event, deferred, consumed) = coalesce_ready_pty_output(
        ActorEvent::PtyOutput(7, b"first".to_vec()),
        &receiver,
        PTY_OUTPUT_BATCH_BYTES,
    );
    assert_eq!(consumed, 2);
    assert!(matches!(
        event,
        ActorEvent::PtyOutput(7, bytes) if bytes == b"firstsecond"
    ));
    assert!(matches!(deferred, Some(ActorEvent::MediaReady)));
    assert!(matches!(
        receiver.try_recv(),
        Ok(ActorEvent::PtyOutput(7, bytes)) if bytes == b"third"
    ));
}

#[test]
fn saturated_media_is_batched_so_actor_control_gets_a_turn() {
    let (media_sender, media_receiver) = mpsc::sync_channel(MEDIA_EVENT_QUEUE);
    for item in 0..(MEDIA_EVENTS_PER_TURN * 2) {
        media_sender.try_send(item).unwrap();
    }
    let (control_sender, control_receiver) = mpsc::sync_channel(1);
    control_sender.try_send("detach").unwrap();

    let mut forwarded = Vec::new();
    assert!(drain_ready_batch(
        &media_receiver,
        MEDIA_EVENTS_PER_TURN,
        |item| forwarded.push(item)
    ));

    assert_eq!(forwarded.len(), MEDIA_EVENTS_PER_TURN);
    assert_eq!(
        control_receiver.try_recv().unwrap(),
        "detach",
        "a saturated or continuously refilled media receiver must yield to actor control"
    );
    assert!(
        media_receiver.try_recv().is_ok(),
        "the test must leave media queued rather than accidentally exercising exhaustion"
    );
}

#[test]
fn plugin_event_replay_is_capacity_bounded_and_reports_direct_eviction_gap() {
    let mut journal = PluginEventJournal::default();
    for sequence in 1..=1_100 {
        journal.push(test_plugin_event(sequence, 8));
    }

    let replay = journal.replay(0, 1_100, 63);
    assert_eq!(replay.len(), 63);
    assert_eq!(
        replay.first(),
        Some(&PluginEventEnvelope::Gap {
            from_sequence: 1,
            to_sequence: 1_038,
        })
    );
    assert_eq!(event_sequence(replay.last().unwrap()), Some(1_100));
}

#[test]
fn plugin_event_journal_stays_within_entry_and_byte_limits_under_firehose() {
    let mut journal = PluginEventJournal::default();
    for sequence in 1..=4_096 {
        journal.push(test_plugin_event(sequence, 8 * 1024));
    }

    assert!(journal.len() <= PLUGIN_EVENT_JOURNAL);
    assert!(journal.bytes <= PLUGIN_EVENT_JOURNAL_BYTES);
    let replay = journal.replay(0, 4_096, 63);
    assert!(replay.len() <= 63);
    assert!(matches!(
        replay.first(),
        Some(PluginEventEnvelope::Gap { .. })
    ));
    assert_eq!(event_sequence(replay.last().unwrap()), Some(4_096));
}

fn test_plugin_event(sequence: u64, payload_bytes: usize) -> PluginEventEnvelope {
    PluginEventEnvelope::Event {
        sequence,
        name: "pane.screen_changed".into(),
        payload: serde_json::json!({"padding": "x".repeat(payload_bytes)}),
        context: vvmux_plugin_api::InvocationContext {
            correlation_id: format!("correlation-{sequence}"),
            causation_id: format!("cause-{sequence}"),
            causation_depth: 0,
            source: "session".into(),
            session_instance: "session-a".into(),
            pane_id: Some(1),
            tab_id: Some(1),
            deadline_unix_ms: 0,
        },
    }
}

#[test]
fn osc52_selections_map_onto_the_single_copy_buffer() {
    for selection in *b"cps" {
        assert!(is_supported_clipboard_selection(selection));
    }
    for selection in *b"q0?" {
        assert!(!is_supported_clipboard_selection(selection));
    }
}

#[test]
fn osc52_store_requires_policy_focus_attachment_and_a_known_selection() {
    use crate::config::Osc52;

    let cases = [
        (Osc52::OnlyCopy, true, true, b'c', true),
        (Osc52::CopyPaste, true, true, b'p', true),
        (Osc52::CopyPaste, true, true, b's', true),
        (Osc52::OnlyCopy, false, true, b'c', false),
        (Osc52::OnlyCopy, true, false, b'c', false),
        (Osc52::Disabled, true, true, b'c', false),
        (Osc52::OnlyPaste, true, true, b'c', false),
        (Osc52::OnlyCopy, true, true, b'q', false),
    ];
    for (policy, focused, attached, selection, expected) in cases {
        assert_eq!(
            clipboard_store_allowed(policy, focused, attached, selection),
            expected,
            "policy={policy:?} focused={focused} attached={attached} selection={selection:?}"
        );
    }
}

#[test]
fn osc52_load_reply_uses_request_selection_terminator_and_copy_buffer() {
    assert_eq!(
        osc52_reply(b'c', "héllo".as_bytes(), "\x1b\\"),
        b"\x1b]52;c;aMOpbGxv\x1b\\"
    );
    assert_eq!(
        osc52_reply(b'p', b"hello", "\x07"),
        b"\x1b]52;p;aGVsbG8=\x07"
    );
}

#[test]
fn bracketed_paste_cannot_inject_terminator() {
    assert_eq!(sanitize_bracketed_paste(b"a\x1b[201~b"), b"a\x1b[201;~b");
}

#[test]
fn pane_selection_clicks_are_counted_only_at_one_cell_within_the_interval() {
    let start = Instant::now();
    let first = MouseClickTracker::next(None, 7, (2, 3), start);
    let second =
        MouseClickTracker::next(Some(first), 7, (2, 3), start + Duration::from_millis(100));
    let third =
        MouseClickTracker::next(Some(second), 7, (2, 3), start + Duration::from_millis(200));
    assert_eq!((first.count, second.count, third.count), (1, 2, 3));
    assert_eq!(
        MouseClickTracker::next(Some(third), 7, (2, 3), start + Duration::from_millis(300),).count,
        1,
        "a fourth click begins a new sequence"
    );
    assert_eq!(
        MouseClickTracker::next(Some(second), 7, (2, 4), start + Duration::from_millis(200),).count,
        1,
        "moving to another cell resets the sequence"
    );
    assert_eq!(
        MouseClickTracker::next(Some(second), 7, (2, 3), start + Duration::from_millis(700),).count,
        1,
        "an expired sequence resets"
    );
}

#[test]
fn pane_selection_clamps_pointer_motion_to_the_captured_content_rectangle() {
    let content = Rect {
        x: 1,
        y: 2,
        width: 4,
        height: 3,
    };
    assert_eq!(mouse_selection_cell(content, 1, 2, 0), Some((0, 0)));
    assert_eq!(
        mouse_selection_cell(content, 40, 20, 0),
        Some((2, 3)),
        "motion over a right-hand pane stays at the origin pane's bottom-right cell"
    );
    assert_eq!(
        mouse_selection_cell(content, 0, 0, 2),
        Some((-2, 0)),
        "copy-view coordinates retain their history offset"
    );
}

#[test]
fn child_mouse_keeps_normal_input_and_copy_mode_selects_without_shift() {
    let press = MouseEvent {
        button: 0,
        x: 3,
        y: 4,
        kind: MouseKind::Press,
        shift: false,
        alt: false,
        ctrl: false,
    };
    let mut modes = TerminalModes::default();
    assert!(starts_mouse_selection(press, false, modes));
    modes.mouse_clicks = true;
    assert!(!starts_mouse_selection(press, false, modes));
    assert!(starts_mouse_selection(press, true, modes));
    for mouse_clicks in [false, true] {
        modes.mouse_clicks = mouse_clicks;
        for copy_mode in [false, true] {
            assert!(!starts_mouse_selection(
                MouseEvent {
                    shift: true,
                    ..press
                },
                copy_mode,
                modes
            ));
        }
    }
}

#[test]
fn pane_selection_runs_are_bounded_and_reverse_direction_is_equivalent() {
    let terminal = Terminal::new(3, 4, 0);
    let forward = MouseSelection {
        start: (0, 2),
        end: (2, 1),
        mode: MouseSelectionMode::Character,
    };
    let backward = MouseSelection {
        start: forward.end,
        end: forward.start,
        ..forward
    };
    let expected = vec![(0, 2, 2), (1, 0, 4), (2, 0, 2)];
    assert_eq!(mouse_selection_runs(&terminal, forward, 0, 4, 3), expected);
    assert_eq!(mouse_selection_runs(&terminal, backward, 0, 4, 3), expected);

    let line = MouseSelection {
        start: (0, 3),
        end: (1, 1),
        mode: MouseSelectionMode::Line,
    };
    assert_eq!(
        mouse_selection_runs(&terminal, line, 0, 4, 3),
        [(0, 0, 4), (1, 0, 4)]
    );
}

#[test]
fn word_selection_highlights_and_copies_the_whole_token() {
    let mut terminal = Terminal::new(2, 40, 0);
    terminal.feed(b"alpha (src/main.rs) bravo");
    let select = |column| MouseSelection {
        start: (0, column),
        end: (0, column),
        mode: MouseSelectionMode::Word,
    };
    assert_eq!(extract_mouse_selection(&terminal, select(4)), b"alpha");
    assert_eq!(
        extract_mouse_selection(&terminal, select(12)),
        b"src/main.rs"
    );
    assert_eq!(
        mouse_selection_runs(&terminal, select(12), 0, 40, 2),
        [(0, 7, 11)]
    );
    assert_eq!(extract_mouse_selection(&terminal, select(18)), b")");
    assert_eq!(
        mouse_selection_runs(&terminal, select(5), 0, 40, 2),
        [(0, 5, 1)]
    );

    let drag = MouseSelection {
        end: (0, 21),
        ..select(2)
    };
    assert_eq!(extract_mouse_selection(&terminal, drag), b"alpha");
    assert_eq!(
        mouse_selection_runs(&terminal, drag, 0, 40, 2),
        [(0, 0, 5)],
        "a word selection stays on the clicked word when the pointer moves"
    );
}

#[test]
fn word_selection_keeps_wide_and_combining_characters_whole() {
    let mut terminal = Terminal::new(2, 20, 0);
    terminal.feed("a界e\u{301}界 z".as_bytes());
    let cell = normalize_mouse_selection_cell(&terminal, (0, 2));
    let selection = MouseSelection {
        start: cell,
        end: cell,
        mode: MouseSelectionMode::Word,
    };
    assert_eq!(
        extract_mouse_selection(&terminal, selection),
        "a界e\u{301}界".as_bytes()
    );
    assert_eq!(
        mouse_selection_runs(&terminal, selection, 0, 20, 2),
        [(0, 0, 6)]
    );
}

#[test]
fn word_selection_crosses_soft_wraps_and_scrollback_but_stops_at_hard_breaks() {
    let mut terminal = Terminal::new(2, 4, 10);
    terminal.feed(b"abcdefgh\r\nijkl");
    let selection = MouseSelection {
        start: (0, 1),
        end: (0, 1),
        mode: MouseSelectionMode::Word,
    };
    assert_eq!(terminal.line_wrapped(-1), Some(true));
    assert_eq!(terminal.line_wrapped(0), Some(false));
    assert_eq!(
        mouse_selection_bounds(&terminal, selection),
        ((-1, 0), (0, 3))
    );
    assert_eq!(extract_mouse_selection(&terminal, selection), b"abcdefgh");
    assert_eq!(
        mouse_selection_runs(&terminal, selection, 1, 4, 2),
        [(0, 0, 4), (1, 0, 4)]
    );
    let next_line = MouseSelection {
        start: (1, 0),
        end: (1, 0),
        ..selection
    };
    assert_eq!(extract_mouse_selection(&terminal, next_line), b"ijkl");
}

#[test]
fn pane_selection_keeps_wide_glyphs_whole() {
    let mut terminal = Terminal::new(1, 4, 0);
    terminal.feed("界x".as_bytes());
    assert_eq!(
        normalize_mouse_selection_cell(&terminal, (0, 1)),
        (0, 0),
        "clicking the continuation addresses the leading cell"
    );
    let selection = MouseSelection {
        start: (0, 0),
        end: (0, 0),
        mode: MouseSelectionMode::Character,
    };
    assert_eq!(
        mouse_selection_runs(&terminal, selection, 0, 4, 1),
        [(0, 0, 2)]
    );
    assert_eq!(
        extract_mouse_selection(&terminal, selection),
        "界".as_bytes()
    );
}

#[test]
fn pane_selection_preserves_tabs_and_combining_text_and_trims_padding() {
    let mut terminal = Terminal::new(1, 12, 0);
    terminal.feed("e\u{301}\tb  ".as_bytes());
    let selection = MouseSelection {
        start: (0, 0),
        end: (0, 11),
        mode: MouseSelectionMode::Line,
    };
    assert_eq!(
        extract_mouse_selection(&terminal, selection),
        "e\u{301}\tb".as_bytes()
    );
}

#[test]
fn mouse_selection_survives_output_that_does_not_scroll() {
    let mut terminal = Terminal::new(4, 20, 10);
    terminal.feed(b"one\r\ntwo\r\nthree\r\nfour");
    let selection = MouseSelection {
        start: (1, 0),
        end: (1, 2),
        mode: MouseSelectionMode::Character,
    };
    // A plain redraw that rewrites cells without scrolling.
    let events = terminal.feed(b"\x1b[2;1HTWO");
    assert_eq!(
        pane_mouse_selection_after_output(Some(selection), &events, false, terminal.history_len()),
        Some(selection)
    );
}

#[test]
fn mouse_selection_rotates_with_scrollback_scroll() {
    let mut terminal = Terminal::new(4, 20, 10);
    terminal.feed(b"one\r\ntwo\r\nthree\r\nfour");
    let selection = MouseSelection {
        start: (1, 0),
        end: (2, 3),
        mode: MouseSelectionMode::Character,
    };
    let text = extract_mouse_selection(&terminal, selection);

    // One full-screen line scrolls into history; both endpoints shift up with their text.
    let events = terminal.feed(b"\r\nx");
    let rotated =
        pane_mouse_selection_after_output(Some(selection), &events, false, terminal.history_len())
            .unwrap();
    assert_eq!(
        rotated,
        MouseSelection {
            start: (0, 0),
            end: (1, 3),
            mode: MouseSelectionMode::Character,
        }
    );
    assert_eq!(extract_mouse_selection(&terminal, rotated), text);
}

#[test]
fn mouse_selection_partially_scrolled_into_history_is_kept() {
    let mut terminal = Terminal::new(4, 20, 10);
    terminal.feed(b"one\r\ntwo\r\nthree\r\nfour");
    let selection = MouseSelection {
        start: (0, 0),
        end: (1, 3),
        mode: MouseSelectionMode::Character,
    };

    // After the scroll, row -1 lives in history and row 0 on screen; both resolve.
    let events = terminal.feed(b"\r\nx");
    assert_eq!(
        pane_mouse_selection_after_output(Some(selection), &events, false, terminal.history_len()),
        Some(MouseSelection {
            start: (-1, 0),
            end: (0, 3),
            mode: MouseSelectionMode::Character,
        })
    );
}

#[test]
fn mouse_selection_drops_when_scrolled_past_retained_history() {
    let mut terminal = Terminal::new(2, 20, 2);
    terminal.feed(b"one\r\ntwo");
    let selection = MouseSelection {
        start: (0, 0),
        end: (0, 2),
        mode: MouseSelectionMode::Character,
    };

    let mut events = Vec::new();
    for _ in 0..6 {
        events.extend(terminal.feed(b"\r\nx"));
    }
    // The selected line was evicted from the two-line scrollback long ago.
    assert_eq!(
        pane_mouse_selection_after_output(Some(selection), &events, false, terminal.history_len()),
        None
    );
}

#[test]
fn mouse_selection_drops_only_when_region_scroll_intersects_it() {
    // Scroll region rows 2..4 (0-based 1..3) on a 4-row terminal.
    let mut terminal = Terminal::new(4, 20, 10);
    terminal.feed(b"\x1b[2;4r\x1b[4;1H");
    let events = terminal.feed(b"\r\n");

    let inside = MouseSelection {
        start: (2, 0),
        end: (2, 3),
        mode: MouseSelectionMode::Character,
    };
    assert_eq!(
        pane_mouse_selection_after_output(Some(inside), &events, false, terminal.history_len()),
        None,
        "a selection inside the scrolled region now points at different text"
    );

    let above = MouseSelection {
        start: (0, 0),
        end: (0, 3),
        mode: MouseSelectionMode::Character,
    };
    assert_eq!(
        pane_mouse_selection_after_output(Some(above), &events, false, terminal.history_len()),
        Some(above),
        "rows outside the scrolled region keep their coordinates"
    );
}

#[test]
fn mouse_selection_drops_on_screen_clear_and_alt_screen_switch() {
    let selection = MouseSelection {
        start: (0, 0),
        end: (1, 3),
        mode: MouseSelectionMode::Character,
    };

    assert_eq!(
        pane_mouse_selection_after_output(
            Some(selection),
            &[TerminalEvent::Clear { alternate: false }],
            false,
            10,
        ),
        None
    );

    let mut terminal = Terminal::new(4, 20, 10);
    terminal.feed(b"one\r\ntwo");
    let events = terminal.feed(b"\x1b[?1049h");
    assert_eq!(
        pane_mouse_selection_after_output(Some(selection), &events, true, terminal.history_len()),
        None
    );
}

#[test]
fn triple_click_copies_visible_rows_instead_of_joining_soft_wraps() {
    let mut terminal = Terminal::new(3, 4, 0);
    terminal.feed(b"abcdefgh");
    assert!(terminal.line_wrapped(0).unwrap());

    let first_row = MouseSelection {
        start: (0, 2),
        end: (0, 2),
        mode: MouseSelectionMode::Line,
    };
    assert_eq!(extract_mouse_selection(&terminal, first_row), b"abcd");

    let two_rows = MouseSelection {
        end: (1, 0),
        ..first_row
    };
    assert_eq!(extract_mouse_selection(&terminal, two_rows), b"abcd\nefgh");

    let character = MouseSelection {
        start: (0, 0),
        end: (1, 3),
        mode: MouseSelectionMode::Character,
    };
    assert_eq!(
        extract_mouse_selection(&terminal, character),
        b"abcdefgh",
        "ordinary selection preserves the existing soft-wrap copy semantics"
    );
}

#[test]
fn a_new_bridge_instance_accepts_a_lower_local_revision_without_regressing_wait_sequence() {
    assert!(bridge_apply_is_current(Some(11), 40, 73, 12, 40, 1));
    assert!(
        !bridge_apply_is_current(Some(11), 40, 73, 11, 40, 72),
        "the current bridge must still reject its own stale local acknowledgement"
    );
    assert_eq!(next_outer_compatibility_revision(73, 1), 74);
    assert_eq!(next_outer_compatibility_revision(4, 9), 9);
}

#[cfg(windows)]
#[test]
fn outer_bracketed_paste_transitions_are_authoritative_and_deduplicated() {
    assert_eq!(
        bracketed_paste_transition(None, false),
        Some(DISABLE_BRACKETED_PASTE)
    );
    assert_eq!(bracketed_paste_transition(Some(false), false), None);
    assert_eq!(
        bracketed_paste_transition(Some(false), true),
        Some(ENABLE_BRACKETED_PASTE)
    );
    assert_eq!(bracketed_paste_transition(Some(true), true), None);
    assert_eq!(
        bracketed_paste_transition(Some(true), false),
        Some(DISABLE_BRACKETED_PASTE)
    );

    let mut enabled = b"render".to_vec();
    prepend_bracketed_paste_transition(&mut enabled, ENABLE_BRACKETED_PASTE);
    assert_eq!(enabled, b"\x1b[?2004hrender");

    let mut disabled = Vec::new();
    prepend_bracketed_paste_transition(&mut disabled, DISABLE_BRACKETED_PASTE);
    assert_eq!(disabled, b"\x1b[?2004l");
}

#[test]
fn display_is_bounded() {
    let with_status = normalized_display(DisplayMetrics::default(), TabView::Bottom);
    assert_eq!((with_status.columns, with_status.rows), (10, 5));
    let without_status = normalized_display(DisplayMetrics::default(), TabView::Hidden);
    assert_eq!((without_status.columns, without_status.rows), (10, 4));
}

#[test]
fn pixel_mouse_is_hit_tested_in_cells_but_forwarded_in_local_pixels() {
    let display = DisplayMetrics {
        columns: 80,
        rows: 24,
        cell_width: 10,
        cell_height: 20,
    };
    let pixel_mouse = MouseEvent {
        button: 0,
        x: 155,
        y: 130,
        kind: MouseKind::Press,
        shift: false,
        alt: false,
        ctrl: false,
    };
    let cell_mouse = pixel_mouse_to_cells(pixel_mouse, display);
    assert_eq!((cell_mouse.x, cell_mouse.y), (15, 6));

    let content = Rect {
        x: 4,
        y: 2,
        width: 30,
        height: 10,
    };
    assert_eq!(
        application_mouse_coordinates(
            cell_mouse,
            Some((pixel_mouse.x, pixel_mouse.y)),
            content,
            display,
            true,
        ),
        (116, 91),
        "pane-local SGR-Pixels coordinates stay one-based and preserve sub-cell position"
    );
    assert_eq!(
        application_mouse_coordinates(cell_mouse, None, content, display, false),
        (12, 5),
        "cell-coordinate clients keep the existing SGR cell report"
    );
}

fn tab_with_floats() -> Tab {
    let area = Rect {
        x: 0,
        y: 0,
        width: 80,
        height: 23,
    };
    let mut tree = TiledNode::leaf(1);
    tree.split(1, 2, crate::ipc::Axis::Horizontal, area)
        .unwrap();
    let mut floating = FloatingLayer::default();
    floating.insert(10, area, FloatOrigin::sized(60, 60));
    floating.insert(11, area, FloatOrigin::sized(40, 40));
    floating.set_pinned(11, true);
    Tab {
        id: 1,
        name: None,
        tree: Some(tree),
        floating,
        focused: 1,
        last_focused_tiled: Some(1),
        zoomed: None,
        sync_input: false,
    }
}

fn status_tab(id: u64, name: Option<&str>) -> Tab {
    Tab {
        id,
        name: name.map(ToOwned::to_owned),
        tree: Some(TiledNode::leaf(id)),
        floating: FloatingLayer::default(),
        focused: id,
        last_focused_tiled: Some(id),
        zoomed: None,
        sync_input: false,
    }
}

#[test]
fn status_lists_only_numbered_tabs_and_marks_the_active_one() {
    let tabs = [
        status_tab(41, Some("dev")),
        status_tab(99, None),
        status_tab(7, Some("logs\nprod")),
    ];
    let status = tab_status_text(&tabs, 1, 80);
    assert_eq!(status, " 1:dev [2] 3:logs prod ");
    assert!(!status.contains("id:"));
    assert!(!status.contains("rev:"));
    assert!(!status.contains("vvmux:"));
}

#[test]
fn narrow_tab_status_keeps_the_active_segment_visible() {
    let tabs = [
        status_tab(1, Some("one")),
        status_tab(2, Some("two")),
        status_tab(3, Some("three")),
        status_tab(4, Some("four")),
    ];
    let status = tab_status_text(&tabs, 2, 14);
    assert!(status.contains("[3:three]"), "{status:?}");
    assert!(
        status.contains('<'),
        "left overflow must be visible: {status:?}"
    );
    assert!(
        status.contains('>'),
        "right overflow must be visible: {status:?}"
    );
    assert!(status.chars().count() <= 14);

    let long = [
        status_tab(1, Some("one")),
        status_tab(2, Some("a-name-that-is-much-too-long")),
        status_tab(3, Some("three")),
    ];
    let status = tab_status_text(&long, 1, 14);
    assert!(status.contains('<'), "{status:?}");
    assert!(status.contains('>'), "{status:?}");
    assert!(status.contains('[') && status.contains(']'), "{status:?}");
    assert!(status.chars().count() <= 14);
}

#[test]
fn status_click_targets_follow_visible_labels_and_stable_tab_ids() {
    let tabs = [
        status_tab(41, Some("dev work")),
        status_tab(99, None),
        status_tab(7, Some("logs")),
    ];
    let (text, targets) = tab_status_layout(&tabs, 1, 80);
    assert_eq!(text, " 1:dev work [2] 3:logs ");
    assert_eq!(targets, vec![(1..11, 41), (12..15, 99), (16..22, 7)]);
    for width in 0..30 {
        let (text, targets) = tab_status_layout(&tabs, 1, width);
        let chars = text.chars().collect::<Vec<_>>();
        for (range, id) in targets {
            assert!(range.end <= usize::from(width));
            assert!(range.end <= chars.len());
            assert!(!chars[range.clone()].contains(&'<'));
            assert!(!chars[range.clone()].contains(&'>'));
            assert!(tabs.iter().any(|tab| tab.id == id));
        }
    }
    let (_, targets) = tab_status_layout(&tabs, 1, 9);
    assert_eq!(targets, vec![(3..6, 99)]);
}

#[test]
fn tab_rename_input_is_bounded_fragment_safe_and_editable() {
    let mut rename = TabRename {
        tab_id: 1,
        value: "dev".into(),
        pending_utf8: Vec::new(),
    };
    assert_eq!(
        apply_tab_rename_input(&mut rename, &[0xc3]),
        LineEditInput::Editing
    );
    assert_eq!(
        apply_tab_rename_input(&mut rename, &[0xa9, 0x7f, b'X']),
        LineEditInput::Editing
    );
    assert_eq!(rename.value, "devX");
    assert_eq!(
        apply_tab_rename_input(&mut rename, b"\r"),
        LineEditInput::Commit
    );

    rename.value = "x".repeat(MAX_TAB_NAME_BYTES);
    assert_eq!(
        apply_tab_rename_input(&mut rename, b"ignored"),
        LineEditInput::Editing
    );
    assert_eq!(rename.value.len(), MAX_TAB_NAME_BYTES);
    assert_eq!(
        apply_tab_rename_input(&mut rename, b"\x1b"),
        LineEditInput::Cancel
    );

    rename.pending_utf8.clear();
    assert_eq!(
        apply_tab_rename_input(&mut rename, &[0xc3, b'\r']),
        LineEditInput::Commit,
        "an invalid UTF-8 lead byte must not swallow Enter"
    );
}

#[test]
fn sync_targets_include_hidden_live_panes_and_exclude_copy_mode() {
    let mut tab = tab_with_floats();
    tab.floating.ordinary_visible = false;
    tab.zoomed = Some(1);
    assert_eq!(
        sync_targets(&tab, &|pane| matches!(pane, 2 | 10)),
        [1, 11],
        "visibility and zoom do not remove live targets, but copy mode does"
    );

    let empty = Tab {
        id: 9,
        name: None,
        tree: None,
        floating: FloatingLayer::default(),
        focused: 99,
        last_focused_tiled: None,
        zoomed: None,
        sync_input: true,
    };
    assert!(sync_targets(&empty, &|_| false).is_empty());
}

#[test]
fn a_removed_sync_target_is_a_no_op() {
    let mut panes = BTreeMap::new();
    assert!(queue_input_targets(&mut panes, &[7], b"ignored").is_empty());
}

#[test]
fn projections_order_tiled_then_ordinary_then_pinned_and_zoom_hides_floats() {
    let area = Rect {
        x: 0,
        y: 0,
        width: 80,
        height: 23,
    };
    let mut tab = tab_with_floats();
    let layers = visible_projections(&tab, area)
        .iter()
        .map(|projection| (projection.pane_id, projection.layer))
        .collect::<Vec<_>>();
    assert_eq!(
        layers,
        [
            (1, PaneLayer::Tiled),
            (2, PaneLayer::Tiled),
            (10, PaneLayer::Floating),
            (11, PaneLayer::Pinned),
        ]
    );

    tab.floating.ordinary_visible = false;
    let visible = visible_projections(&tab, area)
        .iter()
        .map(|projection| projection.pane_id)
        .collect::<Vec<_>>();
    assert_eq!(
        visible,
        [1, 2, 11],
        "hidden ordinary floats leave projection"
    );

    tab.zoomed = Some(1);
    let zoomed = visible_projections(&tab, area);
    assert_eq!(
        zoomed.len(),
        1,
        "zoom hides every other pane, pinned included"
    );
    assert_eq!(zoomed[0].pane_id, 1);
    assert_eq!(zoomed[0].outer, area);

    tab.tree = None;
    tab.zoomed = None;
    tab.floating.ordinary_visible = true;
    let floating_only = visible_projections(&tab, area)
        .iter()
        .map(|projection| projection.pane_id)
        .collect::<Vec<_>>();
    assert_eq!(floating_only, [10, 11], "floating-only tabs project");
}

#[test]
fn media_pane_priority_is_a_strict_focus_and_layer_prefix() {
    let area = Rect {
        x: 0,
        y: 0,
        width: 80,
        height: 23,
    };
    let mut tab = tab_with_floats();
    let projections = visible_projections(&tab, area);
    assert_eq!(projection_pane_priority(&tab, &projections), [1, 11, 10, 2]);

    tab.set_focus(10);
    let projections = visible_projections(&tab, area);
    assert_eq!(projection_pane_priority(&tab, &projections), [10, 11, 1, 2]);
}

#[test]
fn float_pointer_hit_testing_distinguishes_move_edges_and_corners() {
    let rect = Rect {
        x: 10,
        y: 5,
        width: 20,
        height: 10,
    };
    assert_eq!(
        float_pointer_target(rect, 15, 5, 2),
        Some(FloatPointerTarget::Move)
    );
    assert_eq!(
        float_pointer_target(rect, 10, 5, 2),
        Some(FloatPointerTarget::Resize(EdgeMask {
            left: true,
            top: true,
            ..EdgeMask::default()
        }))
    );
    assert_eq!(
        float_pointer_target(rect, 29, 14, 2),
        Some(FloatPointerTarget::Resize(EdgeMask {
            right: true,
            bottom: true,
            ..EdgeMask::default()
        }))
    );
    assert_eq!(float_pointer_target(rect, 15, 6, 2), None);
}

#[test]
fn fallback_focus_prefers_pinned_then_ordinary_then_tiled() {
    let mut tab = tab_with_floats();
    assert_eq!(tab.fallback_focus(), Some(11), "topmost pinned float first");
    tab.floating.remove(11);
    assert_eq!(
        tab.fallback_focus(),
        Some(10),
        "then topmost visible ordinary float"
    );
    tab.floating.ordinary_visible = false;
    assert_eq!(
        tab.fallback_focus(),
        Some(1),
        "then the last focused tiled pane"
    );
    tab.last_focused_tiled = None;
    assert_eq!(tab.fallback_focus(), Some(1), "then the first tiled leaf");
    tab.tree = None;
    tab.floating.ordinary_visible = true;
    assert_eq!(tab.fallback_focus(), Some(10));
    tab.floating.remove(10);
    assert_eq!(tab.fallback_focus(), None);
    assert!(
        tab.is_empty(),
        "a tab with no tree and no floats is removable"
    );
}

#[test]
fn set_focus_tracks_tiled_history_and_raises_floats() {
    let mut tab = tab_with_floats();
    tab.set_focus(2);
    assert_eq!(tab.last_focused_tiled, Some(2));
    tab.floating.insert(
        12,
        Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 23,
        },
        FloatOrigin::sized(40, 40),
    );
    tab.set_focus(10);
    assert_eq!(
        tab.last_focused_tiled,
        Some(2),
        "focusing a float keeps the tiled fallback"
    );
    assert_eq!(
        tab.floating.pane_ids(),
        [12, 10, 11],
        "explicitly focusing a float raises it within its class"
    );
}

#[test]
fn media_projection_is_invalidated_by_active_layout_changes() {
    let first_tab = MediaProjectionKey {
        virtual_revision: 9,
        layout_revision: 3,
    };
    assert!(should_sync_media(false, None, first_tab));
    assert!(!should_sync_media(false, Some(first_tab), first_tab));

    let second_tab = MediaProjectionKey {
        virtual_revision: 9,
        layout_revision: 4,
    };
    assert!(should_sync_media(false, Some(first_tab), second_tab));
    assert!(should_sync_media(true, Some(second_tab), second_tab));
}

#[test]
fn repeated_identical_displays_are_not_resizes() {
    let display = DisplayMetrics {
        columns: 80,
        rows: 24,
        cell_width: 8,
        cell_height: 16,
    };

    // The first display from a newly attached client is always a change.
    assert!(is_display_change(None, display));

    // A browser re-measurement that reports the same geometry must not relayout: doing so
    // bumps layout_revision and rebuilds the outer Vivid session, which destroys an image
    // that is still being projected.
    assert!(!is_display_change(Some(display), display));

    for changed in [
        DisplayMetrics {
            columns: 81,
            ..display
        },
        DisplayMetrics {
            rows: 25,
            ..display
        },
        DisplayMetrics {
            cell_width: 9,
            ..display
        },
        DisplayMetrics {
            cell_height: 17,
            ..display
        },
    ] {
        assert!(
            is_display_change(Some(display), changed),
            "a genuine geometry change must still resize: {changed:?}"
        );
    }
}

#[test]
fn projection_sync_does_not_duplicate_the_triggering_live_raster() {
    let raster = BridgeSourceKey {
        producer: 3,
        context: 1,
        surface: 7,
        track: 7,
    };
    assert!(!should_replay_retained(
        raster,
        Some(raster),
        false,
        false,
        false
    ));
    assert!(should_replay_retained(raster, None, false, true, false));
    assert!(should_replay_retained(
        raster,
        Some(BridgeSourceKey { track: 8, ..raster }),
        true,
        false,
        false
    ));
    assert!(
        !should_replay_retained(raster, None, true, true, false),
        "an already-presented retained body must not cross IPC again while its outer source \
         remains resident"
    );
    assert!(
        should_replay_retained(raster, None, true, true, true),
        "a recreated outer raster has no pixels even when an older attachment was presented"
    );
}

#[test]
fn recreated_retained_replay_is_projection_and_owner_scoped() {
    let raster = BridgeSourceKey {
        producer: 3,
        context: 1,
        surface: 7,
        track: 7,
    };
    let other_owner = BridgeSourceKey {
        producer: 4,
        ..raster
    };
    let candidates = HashSet::from([raster, other_owner]);

    assert_eq!(
        retained_replays_after_apply(&[raster], &candidates, &HashSet::new(), &HashSet::new()),
        HashSet::from([raster])
    );
    assert!(
        retained_replays_after_apply(&[raster], &HashSet::new(), &HashSet::new(), &HashSet::new())
            .is_empty(),
        "initial outer creation must wait for its following live raster instead of replaying it"
    );
    assert!(
        retained_replays_after_apply(
            &[raster],
            &candidates,
            &HashSet::from([raster]),
            &HashSet::new()
        )
        .is_empty()
    );
    assert!(
        retained_replays_after_apply(
            &[raster],
            &candidates,
            &HashSet::new(),
            &HashSet::from([raster])
        )
        .is_empty()
    );
    assert_eq!(
        retained_replays_after_apply(
            &[raster],
            &HashSet::from([other_owner]),
            &HashSet::new(),
            &HashSet::new()
        ),
        HashSet::new(),
        "a same-numbered source from another producer cannot authorize replay"
    );
}

#[test]
fn composed_retained_raster_becomes_a_self_contained_replay_body() {
    let pixels = Arc::<[u8]>::from([0x10, 0x20, 0x30, 0xff, 0x40, 0x50, 0x60, 0xff]);
    let retained = vivid_sdk::presenter::RetainedRaster {
        epoch: 3,
        frame_id: 19,
        width: 2,
        height: 1,
        pixels: Arc::clone(&pixels),
    };

    let body = retained_raster_body(&retained).unwrap();
    let frame = vivid_protocol::media::parse_full_raster_frame(&body).unwrap();
    assert_eq!(frame.epoch, 3);
    assert_eq!(frame.frame_id, 19);
    assert_eq!(frame.width, 2);
    assert_eq!(frame.height, 1);
    assert_eq!(
        vivid_protocol::media::decode_raster_pixels(frame).unwrap(),
        &*pixels
    );
}

#[test]
fn fragment_ids_are_stable_and_recycle_only_disappeared_rectangles() {
    let left = FixedRect::new(0, 0, 10, 10).unwrap();
    let right = FixedRect::new(20, 0, 10, 10).unwrap();
    let bottom = FixedRect::new(0, 20, 10, 10).unwrap();
    let mut map = FragmentMap::default();
    assert_eq!(map.assign(&[left, right]).unwrap(), [(0, left), (1, right)]);
    assert_eq!(
        map.assign(&[right, left]).unwrap(),
        [(1, right), (0, left)],
        "snapshot ordering cannot renumber unchanged rectangles"
    );
    assert_eq!(
        map.assign(&[right, bottom]).unwrap(),
        [(1, right), (0, bottom)],
        "the disappeared left rectangle returns ID zero to the pool"
    );
    // Inactive-tab hiding does not call assign at all, so the next identical geometry keeps
    // both assignments.
    assert_eq!(
        map.assign(&[right, bottom]).unwrap(),
        [(1, right), (0, bottom)]
    );
    assert_eq!(
        FragmentMap::default().assign(&[right]).unwrap(),
        [(0, right)],
        "destroy/recreate starts with a fresh logical-node map"
    );
}

#[test]
fn maximum_fragment_scene_stays_bounded_across_drag_storms() {
    let mut maps = (0..256)
        .map(|logical| (logical, FragmentMap::default()))
        .collect::<HashMap<_, _>>();
    for step in 0..1000_i64 {
        for (logical, map) in &mut maps {
            let fragments = (0..MAX_NODE_FRAGMENTS)
                .map(|fragment| {
                    FixedRect::new(step + fragment as i64 * 3, i64::from(*logical) * 2, 2, 1)
                        .unwrap()
                })
                .collect::<Vec<_>>();
            let assignments = map.assign(&fragments).unwrap();
            assert_eq!(assignments.len(), MAX_NODE_FRAGMENTS);
            assert!(
                assignments
                    .iter()
                    .all(|(id, _)| usize::from(*id) < MAX_NODE_FRAGMENTS)
            );
        }
    }
    assert_eq!(maps.len(), 256);
    assert_eq!(
        maps.values().map(|map| map.rectangles.len()).sum::<usize>(),
        256 * MAX_NODE_FRAGMENTS,
        "recycled drag geometry cannot grow fragment maps monotonically"
    );
    assert_eq!(
        MAX_PROJECTED_NODES / MAX_NODE_FRAGMENTS,
        32,
        "the strict global prefix admits exactly 32 eight-fragment nodes"
    );
}

fn projected_test_node(width: i64, height: i64) -> vivid_sdk::presenter::SceneNode {
    vivid_sdk::presenter::SceneNode {
        producer: 3,
        pane: 7,
        config: vivid_sdk::presenter::SceneNodeConfig {
            node: vivid_sdk::presenter::NodeConfig {
                node_id: 9,
                track: BridgeSourceKey {
                    producer: 3,
                    context: 1,
                    surface: 4,
                    track: 5,
                },
                x: 0,
                y: 0,
                width,
                height,
                z_index: 2,
                visible: true,
                anchor_id: None,
            },
            clip: None,
        },
    }
}

#[test]
fn logical_node_projection_clips_and_subtracts_higher_outer_rectangles() {
    let pane = Rect {
        x: 1,
        y: 2,
        width: 10,
        height: 8,
    };
    let area = Rect {
        x: 0,
        y: 0,
        width: 20,
        height: 12,
    };
    let occluder = from_cells(Rect {
        x: 4,
        y: 3,
        width: 3,
        height: 4,
    })
    .unwrap();
    let node = projected_test_node(20_i64 << 32, 20_i64 << 32);
    let projected = project_logical_node(&node, pane, area, &[occluder]).unwrap();
    assert!(!projected.fragments.is_empty());
    for fragment in &projected.fragments {
        assert!(intersect(fragment.clip, occluder).is_none());
        assert_eq!(fragment.node.x, i64::from(pane.x) << 32);
        assert_eq!(fragment.node.y, i64::from(pane.y) << 32);
    }
    let pane_area = (i128::from(pane.width) * i128::from(pane.height)) << 64;
    let overlap = intersect(from_cells(pane).unwrap(), occluder).unwrap();
    let visible_area = projected
        .fragments
        .iter()
        .map(|fragment| i128::from(fragment.clip.width) * i128::from(fragment.clip.height))
        .sum::<i128>();
    assert_eq!(
        visible_area,
        pane_area - i128::from(overlap.width) * i128::from(overlap.height)
    );

    let covered = project_logical_node(&node, pane, area, &[from_cells(pane).unwrap()]).unwrap();
    assert!(covered.fragments.is_empty());
}

#[test]
fn logical_node_fragment_limit_is_atomic() {
    let pane = Rect {
        x: 0,
        y: 0,
        width: 24,
        height: 5,
    };
    let node = projected_test_node(24_i64 << 32, 5_i64 << 32);
    let occluders = (1..18)
        .step_by(2)
        .map(|x| {
            from_cells(Rect {
                x,
                y: 1,
                width: 1,
                height: 3,
            })
            .unwrap()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        project_logical_node(&node, pane, pane, &occluders).unwrap_err(),
        ProjectionIssue::FragmentLimit
    );
}

#[test]
fn client_cursor_keys_follow_pane_application_cursor_mode() {
    let mut modes = TerminalModes::default();
    let mut in_paste = false;
    let typed = b"a\x1b[A\x1b[B\x1b[C\x1b[D\x1b[H\x1b[F\x1b[1;5A\x1b[3~";
    assert!(matches!(
        key_input_bytes(typed, modes, &mut in_paste),
        Cow::Borrowed(bytes) if bytes == typed
    ));

    // Python's REPL sends smkx and then matches only terminfo's `ESC O A` for Up.
    modes.application_cursor = true;
    assert_eq!(
        &*key_input_bytes(typed, modes, &mut in_paste),
        b"a\x1bOA\x1bOB\x1bOC\x1bOD\x1bOH\x1bOF\x1b[1;5A\x1b[3~"
    );

    // Pasted bytes are text, even when the paste is split across input messages.
    assert_eq!(
        &*key_input_bytes(b"\x1b[A\x1b[200~x\x1b[A", modes, &mut in_paste),
        b"\x1bOA\x1b[200~x\x1b[A"
    );
    assert!(in_paste);
    assert_eq!(
        &*key_input_bytes(b"\x1b[B\x1b[201~\x1b[B", modes, &mut in_paste),
        b"\x1b[B\x1b[201~\x1bOB"
    );
    assert!(!in_paste);

    modes.keyboard_flags = 8;
    assert_eq!(
        &*key_input_bytes(b"\x1b[A", modes, &mut in_paste),
        b"\x1b[A"
    );
}

#[test]
fn automation_keys_honor_cursor_keypad_modifiers_and_reject_unknown_values() {
    let mut modes = TerminalModes::default();
    assert_eq!(
        encode_automation_key("ArrowUp", &[], modes).unwrap(),
        b"\x1b[A"
    );
    modes.application_cursor = true;
    assert_eq!(
        encode_automation_key("ArrowUp", &[], modes).unwrap(),
        b"\x1bOA"
    );
    modes.application_keypad = true;
    assert_eq!(
        encode_automation_key("Keypad7", &[], modes).unwrap(),
        b"\x1bOw"
    );
    assert_eq!(
        encode_automation_key("c", &["Ctrl".into()], modes).unwrap(),
        b"\x03"
    );
    assert_eq!(
        encode_automation_key("Tab", &["Shift".into()], modes).unwrap(),
        b"\x1b[Z"
    );
    encode_automation_key("NoSuchKey", &[], modes).unwrap_err();
    encode_automation_key("x", &["Hyper".into()], modes).unwrap_err();
}

#[test]
fn automation_raw_request_limits_are_enforced() {
    assert!(
        validate_automation_method(&AutomationMethod::Action(Action::CopyInput(vec![b'q'])))
            .is_err()
    );
    validate_automation_method(&AutomationMethod::Action(Action::Plugin(
        "plugin:dev.example/run".into(),
    )))
    .unwrap();
    assert!(
        validate_automation_method(&AutomationMethod::Action(Action::Plugin(
            "dev.example/run".into()
        )))
        .is_err()
    );
    assert!(
        validate_automation_method(&AutomationMethod::Key {
            key: "x".into(),
            modifiers: Vec::new(),
            repeat: 0,
            report: false,
        })
        .is_err()
    );
    assert!(
        validate_automation_method(&AutomationMethod::GetText {
            rows: Some(1001),
            source: TextSource::RecentUnwrapped,
        })
        .is_err()
    );
    // A row count is meaningless for the viewport and for the fixed classification snapshot,
    // so it is refused rather than silently ignored.
    for source in [TextSource::Visible, TextSource::Detection] {
        assert!(
            validate_automation_method(&AutomationMethod::GetText {
                rows: Some(10),
                source,
            })
            .is_err()
        );
        validate_automation_method(&AutomationMethod::GetText { rows: None, source }).unwrap();
    }
    assert!(
        validate_automation_method(&AutomationMethod::GetGrid {
            start_line: Some(0),
            row_count: None,
            since_screen: None,
        })
        .is_err()
    );
    assert!(
        validate_automation_method(&AutomationMethod::Search {
            pattern: "x".repeat(crate::search::MAX_PATTERN_BYTES + 1),
            regex: false,
            direction: SearchDirection::Forward,
            start_line: None,
            start_column: None,
            limit: 1,
        })
        .is_err()
    );
    assert!(
        validate_automation_method(&AutomationMethod::Search {
            pattern: "x".into(),
            regex: false,
            direction: SearchDirection::Forward,
            start_line: None,
            start_column: Some(1),
            limit: 1001,
        })
        .is_err()
    );
    assert!(
        validate_automation_method(&AutomationMethod::WaitText {
            text: "x".into(),
            regex: false,
            after_screen: None,
            timeout_ms: 0,
        })
        .is_err()
    );
    assert!(
        validate_automation_method(&AutomationMethod::TraceMedia {
            after_sequence: None,
            limit: 0,
            timeout_ms: 0,
            filter: MediaTraceFilter::default(),
        })
        .is_err()
    );
    assert!(
        validate_automation_method(&AutomationMethod::TraceMedia {
            after_sequence: None,
            limit: 32,
            timeout_ms: 0,
            filter: MediaTraceFilter {
                context_id: Some(4),
                ..MediaTraceFilter::default()
            },
        })
        .is_err()
    );
    validate_automation_method(&AutomationMethod::TraceMedia {
        after_sequence: None,
        limit: 32,
        timeout_ms: 0,
        filter: MediaTraceFilter::default(),
    })
    .unwrap();
}
