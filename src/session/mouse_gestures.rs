//! Timed automation gestures. Only the session actor moves the pointer.

use super::{
    AutomationError, AutomationReplyTarget, AutomationWaitKind, AutomationWaiter,
    MAX_AUTOMATION_WAITERS, MAX_MOUSE_PATH_POINTS, MouseEvent, MouseKind, MouseRequest, PaneId,
    Rect, SessionActor, deadline,
};
use crate::ipc::MouseRoute;
use std::time::{Duration, Instant};
use vvmux_terminal::pty::PtyWritePermit;

type Point = (MouseEvent, Option<(u16, u16)>);

/// Bound both per-turn work and outstanding point storage.
const MAX_PACED_GESTURES: usize = 16;

pub(super) struct PacedMouse {
    target: AutomationReplyTarget,
    pane: PaneId,
    route: MouseRoute,
    points: Vec<Point>,
    next: usize,
    started: Instant,
    duration: Duration,
    deadline: Instant,
    witness: Option<u64>,
    sequence: u64,
    layout: u64,
    release: Option<PtyWritePermit>,
}

impl PacedMouse {
    fn next_at(&self) -> Instant {
        self.started
            + (self.duration * u32::try_from(self.next).expect("path has at most 1000 points"))
                / u32::try_from(self.points.len() - 1).expect("path has at most 1000 points")
    }
}

impl SessionActor {
    pub(super) fn resolve_mouse_path(
        &self,
        content: Rect,
        request: &MouseRequest,
    ) -> Result<Vec<Point>, AutomationError> {
        if !(2..=MAX_MOUSE_PATH_POINTS).contains(&request.points.len()) {
            return Err(AutomationError::new(
                "invalid_params",
                format!("a mouse path takes 2 through {MAX_MOUSE_PATH_POINTS} points"),
            ));
        }
        request
            .points
            .iter()
            .enumerate()
            .map(|(index, position)| {
                let point = self.resolve_mouse_position(content, *position)?;
                let kind = if index == 0 {
                    MouseKind::Press
                } else if index + 1 == request.points.len() {
                    MouseKind::Release
                } else {
                    MouseKind::Move
                };
                Ok((
                    MouseEvent {
                        x: point.x,
                        y: point.y,
                        kind,
                        button: request.button.code(),
                        shift: request.shift,
                        ctrl: request.ctrl,
                        alt: request.alt,
                    },
                    point.pixels,
                ))
            })
            .collect()
    }

    pub(super) fn route_mux_mouse(&mut self, mut event: MouseEvent, pixels: Option<(u16, u16)>) {
        if let Some((x, y)) = pixels {
            event.x = x;
            event.y = y;
        }
        self.mouse(event, pixels.is_some());
    }

    /// Queue every report of an unpaced application gesture in one write. A full queue then
    /// refuses the entire gesture before the application has seen a press.
    pub(super) fn send_gesture_points(
        &mut self,
        pane: PaneId,
        route: MouseRoute,
        points: Vec<Point>,
    ) -> Result<(), AutomationError> {
        match route {
            MouseRoute::Application => {
                let mut reports = String::new();
                for (event, pixels) in points {
                    reports.push_str(&self.application_mouse_report(pane, event, pixels)?);
                }
                self.panes[&pane]
                    .input
                    .send(reports.as_bytes())
                    .map_err(mouse_input_error)
            }
            MouseRoute::Mux => {
                for (event, pixels) in points {
                    self.route_mux_mouse(event, pixels);
                }
                Ok(())
            }
        }
    }

    fn send_paced_point(
        &mut self,
        g: &mut PacedMouse,
        event: MouseEvent,
        pixels: Option<(u16, u16)>,
    ) -> Result<(), AutomationError> {
        if g.route == MouseRoute::Mux {
            self.route_mux_mouse(event, pixels);
            return Ok(());
        }
        let report = self.application_mouse_report(g.pane, event, pixels)?;
        if event.kind == MouseKind::Release {
            g.release
                .take()
                .expect("application gesture reserves a release before pressing")
                .send(report.as_bytes())
                .map_err(mouse_input_error)
        } else {
            self.panes[&g.pane]
                .input
                .send(report.as_bytes())
                .map_err(mouse_input_error)
        }
    }

    pub(super) fn start_mouse_request(
        &mut self,
        target: AutomationReplyTarget,
        pane: PaneId,
        request: MouseRequest,
        duration_ms: Option<u64>,
        wait_rendered: bool,
        timeout_ms: u64,
    ) {
        let expires = deadline(timeout_ms);
        let witness = if wait_rendered {
            let Some(id) = self
                .witness_client()
                .map(|c| c.id)
                .filter(|_| self.pane_is_visibly_present(pane))
            else {
                self.reply_automation_error(
                    target,
                    AutomationError::new(
                        "unsupported",
                        "mouse render verification requires a visible pane and an attached client",
                    ),
                );
                return;
            };
            if self.automation_waiters.len() >= MAX_AUTOMATION_WAITERS {
                self.reply_automation_error(
                    target,
                    AutomationError::new("limit_exceeded", "too many automation waiters"),
                );
                return;
            }
            Some(id)
        } else {
            None
        };
        if self.mouse_gestures.iter().any(|g| {
            g.pane == pane || (request.route == MouseRoute::Mux && g.route == MouseRoute::Mux)
        }) {
            self.reply_automation_error(
                target,
                AutomationError::new("invalid_state", "another gesture is in progress"),
            );
            return;
        }
        if let Some(ms) = duration_ms {
            if self.mouse_gestures.len() >= MAX_PACED_GESTURES {
                self.reply_automation_error(
                    target,
                    AutomationError::new("limit_exceeded", "too many paced mouse gestures"),
                );
                return;
            }
            let points = match self
                .pane_content_rect(pane)
                .and_then(|area| self.resolve_mouse_path(area, &request))
            {
                Ok(points) => points,
                Err(error) => {
                    self.reply_automation_error(target, error);
                    return;
                }
            };
            // SGR reports contain a u16 button, two u32 coordinates, fixed punctuation and a terminator.
            // Hold a queue item and its maximum 32 bytes until the final release or cancellation.
            let release = if request.route == MouseRoute::Application {
                match self.panes[&pane].input.reserve(32) {
                    Ok(slot) => Some(slot),
                    Err(error) => {
                        self.reply_automation_error(target, mouse_input_error(error));
                        return;
                    }
                }
            } else {
                None
            };
            self.mouse_gestures.push(PacedMouse {
                target,
                pane,
                route: request.route,
                points,
                next: 0,
                started: Instant::now(),
                duration: Duration::from_millis(ms),
                deadline: expires,
                witness,
                sequence: self.session_sequence,
                layout: self.layout_revision,
                release,
            });
            self.poll_mouse_gestures();
        } else {
            match self.automation_mouse(pane, request) {
                Ok(result) => self.finish_mouse(target, pane, witness, expires, result),
                Err(error) => self.reply_automation_error(target, error),
            }
        }
    }

    fn finish_mouse(
        &mut self,
        target: AutomationReplyTarget,
        pane: PaneId,
        witness: Option<u64>,
        expires: Instant,
        result: serde_json::Value,
    ) {
        if let Some(client_id) = witness {
            // A stationary click can change no cells. Still require this witness to process a
            // post-delivery frame; an earlier in-flight acknowledgement cannot satisfy this wait.
            self.session_sequence = self.session_sequence.wrapping_add(1);
            self.force_full = true;
            self.schedule_render();
            self.add_automation_waiter(AutomationWaiter {
                reply: target,
                pane_id: Some(pane),
                deadline: expires,
                kind: AutomationWaitKind::MouseRendered {
                    client_id,
                    after_session: self.session_sequence,
                    result,
                },
            });
        } else {
            self.reply_automation(target, result);
        }
    }

    pub(super) fn next_mouse_deadline(&self) -> Option<Duration> {
        let now = Instant::now();
        self.mouse_gestures
            .iter()
            .map(|g| g.next_at().min(g.deadline).saturating_duration_since(now))
            .min()
    }

    fn release_gesture(&mut self, g: &mut PacedMouse) {
        if g.next == 0 {
            return;
        }
        if g.route == MouseRoute::Mux && g.layout != self.layout_revision {
            self.cancel_pointer_drag(true);
            self.mouse_selection_drag = None;
        } else if self.panes.contains_key(&g.pane) {
            let (mut event, pixels) = g.points[g.next - 1];
            event.kind = MouseKind::Release;
            let _ = self.send_paced_point(g, event, pixels);
        }
    }

    pub(super) fn cancel_pane_gestures(
        &mut self,
        pane: PaneId,
        code: &'static str,
        reason: &'static str,
    ) {
        for mut gesture in std::mem::take(&mut self.mouse_gestures) {
            if gesture.pane == pane {
                self.release_gesture(&mut gesture);
                self.reply_automation_error(gesture.target, AutomationError::new(code, reason));
            } else {
                self.mouse_gestures.push(gesture);
            }
        }
    }

    pub(super) fn cancel_client_gestures(&mut self, client: u64) {
        for mut g in std::mem::take(&mut self.mouse_gestures) {
            if g.target.client_id == client {
                self.release_gesture(&mut g);
            } else {
                self.mouse_gestures.push(g);
            }
        }
    }

    pub(super) fn poll_mouse_gestures(&mut self) {
        let now = Instant::now();
        for mut g in std::mem::take(&mut self.mouse_gestures) {
            let failure = if now >= g.deadline {
                Some(("timeout", "mouse gesture timed out"))
            } else if !self.panes.contains_key(&g.pane) {
                Some(("pane_not_found", "mouse target closed"))
            } else if g.layout != self.layout_revision {
                Some(("invalid_state", "layout changed during gesture"))
            } else if g.witness.is_some_and(|id| !self.clients.contains_key(&id)) {
                Some(("unsupported", "mouse render witness disconnected"))
            } else {
                None
            };
            if let Some((code, reason)) = failure {
                self.release_gesture(&mut g);
                self.reply_automation_error(g.target, AutomationError::new(code, reason));
                continue;
            }
            if now >= g.next_at() {
                let (event, pixels) = g.points[g.next];
                if let Err(error) = self.send_paced_point(&mut g, event, pixels) {
                    if event.kind != MouseKind::Release {
                        self.release_gesture(&mut g);
                    }
                    self.reply_automation_error(g.target, error);
                    continue;
                }
                g.next += 1;
                g.layout = self.layout_revision;
            }
            if g.next == g.points.len() {
                let result = serde_json::json!({"pane_id":g.pane,"events":g.next,"route":g.route,"before_session_sequence":g.sequence,"session_sequence":self.session_sequence});
                self.finish_mouse(g.target, g.pane, g.witness, g.deadline, result);
            } else {
                self.mouse_gestures.push(g);
            }
        }
    }
}

fn mouse_input_error(error: std::io::Error) -> AutomationError {
    let code = if error.kind() == std::io::ErrorKind::WouldBlock {
        "limit_exceeded"
    } else {
        "pty_closed"
    };
    AutomationError::new(code, error.to_string())
}
