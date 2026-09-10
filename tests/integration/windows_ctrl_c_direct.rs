#![cfg(windows)]

use std::path::Path;
use std::time::Duration;

use super::windows_ctrl_c_support::{Console, PROMPT};

#[test]
fn ctrl_c_interrupts_conpty_child_process() {
    let mut console = Console::spawn(Path::new(env!("CARGO_MANIFEST_DIR")));
    assert!(
        console.wait(Duration::from_secs(10), |terminal| {
            terminal.visible_text(0).contains(PROMPT)
        }),
        "no shell prompt:\n{}",
        console.text()
    );
    console.interrupt_ping();
}
