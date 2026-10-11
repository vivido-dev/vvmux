//! Terminal modes and the events a [`Terminal`](crate::Terminal) reports to its owner.

/// The input-affecting modes an application has set, which its host must honor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct TerminalModes {
    /// DECCKM: cursor keys send application (`ESC O`) sequences.
    pub application_cursor: bool,
    /// DECSET 2004: pasted text is wrapped in bracketed-paste markers.
    pub bracketed_paste: bool,
    /// DECSET 1000: report mouse button presses and releases.
    pub mouse_clicks: bool,
    /// DECSET 1002 or 1003: also report mouse motion.
    pub mouse_motion: bool,
    /// DECSET 1006: encode mouse reports in SGR form.
    pub sgr_mouse: bool,
    /// DECSET 1016: report SGR mouse positions in pixels.
    pub sgr_pixels: bool,
    /// DECSET 1004: report focus gained and lost.
    pub focus_reporting: bool,
    /// DECTCEM: the cursor is shown.
    pub cursor_visible: bool,
    /// DECKPAM: the keypad sends application sequences.
    pub application_keypad: bool,
    /// The active Kitty keyboard-protocol flags, or zero.
    pub keyboard_flags: u8,
}

/// Something the host of a [`Terminal`](crate::Terminal) must act on after feeding it output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalEvent {
    /// The grid changed and needs repainting. Reported at most once per batch.
    Damage,
    /// The window title changed; `None` resets it.
    Title(Option<String>),
    /// BEL.
    Bell,
    /// Bytes to write back to the pane, such as replies to status and capability queries.
    PtyWrite(Vec<u8>),
    /// The modes changed; the payload is the complete new set.
    ModeChange(TerminalModes),
    /// A Vivid anchor marker, consumed from the output where it appeared.
    VividMarker {
        /// The marker text, already shape-checked but not yet authenticated.
        marker: String,
        /// The row the marker began on.
        row: usize,
        /// The column the marker began on.
        column: usize,
        /// Whether the alternate screen was active.
        alternate: bool,
    },
    /// A validated Kitty graphics command, consumed from the output.
    KittyGraphics(KittyGraphicsCommand),
    /// The grid scrolled. `lines` is signed (up positive, down negative); `top`/`bottom` bound the
    /// scrolled region. `pushed_to_history` is set only when a full-screen primary-screen scroll
    /// carried lines into scrollback — the only scroll that keeps a viewport-anchored cell on the
    /// same text, which is what selection rotation needs to know.
    GridScroll {
        /// Rows scrolled: positive is up, negative is down.
        lines: i32,
        /// First row of the scrolled region.
        top: usize,
        /// One past the last row of the scrolled region.
        bottom: usize,
        /// Whether the rows scrolled off the top went into scrollback.
        pushed_to_history: bool,
        /// Whether the alternate screen scrolled.
        alternate: bool,
    },
    /// The application cleared the whole screen, so anything anchored to it should go too.
    Clear {
        /// Whether the alternate screen was cleared.
        alternate: bool,
    },
    /// The application switched between the primary and alternate screens.
    ScreenSwap {
        /// Whether the alternate screen is now active.
        alternate: bool,
    },
    /// A pane asked to write the user's clipboard (OSC 52 store). Already decoded and bounded.
    ClipboardStore {
        /// The OSC 52 selection letter, such as `b'c'`.
        selection: u8,
        /// The decoded clipboard contents.
        text: Vec<u8>,
    },
    /// A pane asked to read the user's clipboard (OSC 52 query). The pane's own terminator is
    /// carried back so the reply is framed the way the pane framed its request.
    ClipboardLoad {
        /// The OSC 52 selection letter, such as `b'c'`.
        selection: u8,
        /// The string terminator the request used: BEL or ST.
        terminator: String,
    },
}

/// A validated Kitty graphics command consumed from pane output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KittyGraphicsCommand {
    /// The standard direct-transmission support query. The session answers on the pane PTY.
    Query {
        /// The image ID the reply must carry.
        image_id: u32,
        /// Reply suppression: zero replies always, one replies only on failure.
        ///
        /// The original vvmux `q=2` one-pixel probe is normalized to zero for compatibility.
        quiet: u8,
    },
    /// A command rejected locally, without forwarding it to the host terminal.
    Rejected {
        /// The image ID the reply must carry.
        image_id: u32,
        /// An optional placement ID to echo in the error response.
        placement_id: Option<u32>,
        /// A bounded, printable protocol error and explanation.
        error: &'static str,
    },
    /// A packet safe to forward to a directly attached Kitty-compatible host.
    Packet {
        /// Complete escape sequences, including all chunks of a translated cursor placement.
        bytes: Vec<u8>,
        /// Whether this packet begins a new image transfer.
        starts_transfer: bool,
        /// Whether more chunks of the same transfer follow (`m=1`).
        more: bool,
        /// Acknowledgement sent only after the session accepts the complete packet.
        reply: Option<KittyGraphicsReply>,
    },
}

/// A pane-local acknowledgement for an accepted graphics command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KittyGraphicsReply {
    /// The application's image ID, independent of the host image ID.
    pub image_id: u32,
    /// The application's placement ID, if specified.
    pub placement_id: Option<u32>,
    /// Zero replies always; one replies on error; two suppresses replies.
    pub quiet: u8,
}

impl KittyGraphicsReply {
    /// Encode a success or a caller-supplied protocol error.
    #[must_use]
    pub fn encode(self, error: Option<&str>) -> Option<Vec<u8>> {
        if self.image_id == 0 || self.quiet == 2 || (self.quiet == 1 && error.is_none()) {
            return None;
        }
        let placement = self
            .placement_id
            .map_or_else(String::new, |id| format!(",p={id}"));
        Some(
            format!(
                "\x1b_Gi={}{placement};{}\x1b\\",
                self.image_id,
                error.unwrap_or("OK")
            )
            .into_bytes(),
        )
    }
}
