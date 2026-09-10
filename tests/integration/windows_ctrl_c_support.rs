#![cfg(windows)]

use std::io::Read;
use std::path::Path;
use std::sync::mpsc::{self, Receiver};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use vvmux_terminal::{
    Terminal,
    pty::{PtyControl, PtyInput, PtyProcess},
};

pub const PROMPT: &str = "VVMUX_CTRL_C_READY>";

pub struct Console {
    pub input: PtyInput,
    control: PtyControl,
    receiver: Option<Receiver<Vec<u8>>>,
    reader: Option<JoinHandle<()>>,
    terminal: Terminal,
}

impl Console {
    pub fn spawn(cwd: &Path) -> Self {
        let shell =
            std::env::var_os("COMSPEC").unwrap_or_else(|| "C:\\Windows\\System32\\cmd.exe".into());
        let parts = PtyProcess::spawn_argv(
            &shell,
            &["/D", "/Q"],
            cwd,
            100,
            30,
            &[("PROMPT".into(), "VVMUX_CTRL_C_READY$G".into())],
        )
        .unwrap();
        let mut reader = parts.reader;
        let (sender, receiver) = mpsc::sync_channel(64);
        let thread = std::thread::spawn(move || {
            let mut buffer = [0; 8192];
            while let Ok(read) = reader.read(&mut buffer) {
                if read == 0 || sender.send(buffer[..read].to_vec()).is_err() {
                    break;
                }
            }
        });
        Self {
            input: parts.input,
            control: parts.control,
            receiver: Some(receiver),
            reader: Some(thread),
            terminal: Terminal::new(30, 100, 100),
        }
    }

    pub fn text(&self) -> String {
        self.terminal.visible_text(0)
    }

    pub fn wait(&mut self, timeout: Duration, predicate: impl Fn(&Terminal) -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if predicate(&self.terminal) {
                return true;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            match self.receiver.as_ref().unwrap().recv_timeout(remaining) {
                Ok(chunk) => {
                    self.terminal.feed(&chunk);
                }
                Err(_) => return false,
            }
        }
    }

    pub fn interrupt_ping(&mut self) {
        // Clear the old prompt so only a prompt printed after interruption can satisfy the test.
        // A single Enter avoids leaving an extra blank command queued behind ping.
        self.input.send(b"cls & ping -n 30 127.0.0.1\r").unwrap();
        assert!(
            self.wait(Duration::from_secs(10), |terminal| {
                let text = terminal.visible_text(0);
                text.contains("TTL=") && !text.contains(PROMPT)
            }),
            "ping did not start:\n{}",
            self.text()
        );
        self.input.send(b"\x03").unwrap();
        assert!(
            self.wait(Duration::from_secs(5), |terminal| {
                terminal.visible_text(0).contains(PROMPT)
            }),
            "Ctrl+C did not return to the shell prompt:\n{}",
            self.text()
        );
    }
}

impl Drop for Console {
    fn drop(&mut self) {
        self.receiver.take();
        self.control.terminate_blocking();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

#[test]
fn readiness_matches_rendered_text_across_conpty_redraws() {
    let mut terminal = Terminal::new(30, 100, 0);
    for byte in b"TTL=\x1b[?25l64\r\nVVMUX_CTRL_C_\x1b[?25hREADY>" {
        terminal.feed(&[*byte]);
    }
    let text = terminal.visible_text(0);
    assert!(text.contains("TTL=64"));
    assert!(text.contains(PROMPT));
}
