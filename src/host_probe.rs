//! Bounded host capability discovery and attachment-scoped terminal reply filtering.

// Rust guideline compliant 2026-10-07

use std::ffi::OsStr;
use std::time::{Duration, Instant};

/// Remote hosts get one second; local hosts normally finish at the DA reply immediately.
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_secs(1);
/// Bound typed-ahead storage even when the terminal continuously sends unrelated input.
#[cfg(unix)]
pub(crate) const PROBE_INPUT_LIMIT: usize = 64 * 1024;
/// Replies are tiny; this cap also bounds an unterminated or malformed reply candidate.
const REPLY_LIMIT: usize = 1024;

/// Delete all images, including virtual placements, in the current host screen's store.
pub(crate) const RESET_GRAPHICS: &[u8] = b"\x1b_Ga=d,d=R,x=1,y=4294967295,q=2\x1b\\";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Host {
    Known,
    Ask,
    No,
}

pub(crate) fn classify(term: Option<&OsStr>, vivid_credentials: bool) -> Host {
    match term.and_then(OsStr::to_str) {
        Some("xterm-kitty" | "xterm-ghostty") => Host::Known,
        // A nested vvmux accepts only its own quiet query contract, not the host query.
        Some("vvmux") => Host::No,
        Some("vivido") => Host::Ask,
        _ if vivid_credentials => Host::Ask,
        _ => Host::No,
    }
}

#[cfg(any(unix, test))]
pub(crate) fn query(image_id: u32) -> Vec<u8> {
    format!("\x1b_Gi={image_id},s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\x1b[c").into_bytes()
}

#[derive(Debug, Default)]
pub(crate) struct Probe {
    pub(crate) supported: bool,
    pub(crate) input: Vec<u8>,
    pub(crate) replies: Replies,
}

impl Probe {
    pub(crate) fn known(supported: bool) -> Self {
        Self {
            supported,
            ..Self::default()
        }
    }
}

/// Consume exactly one graphics response for our image and one primary DA sentinel.
///
/// The same state continues after a timeout, including any partial reply. Unrelated bytes retain
/// their order, and once a response is consumed an identical later escape sequence passes through.
#[derive(Debug, Default)]
pub(crate) struct Replies {
    graphics_prefix: Vec<u8>,
    graphics: Option<bool>,
    sentinel: bool,
    pending: Vec<u8>,
    held_since: Option<Instant>,
}

#[derive(Debug, Clone, Copy)]
enum Match {
    Prefix,
    Graphics(bool),
    Sentinel,
    Unrelated,
}

impl Replies {
    #[cfg(any(unix, test))]
    pub(crate) fn new(image_id: u32) -> Self {
        Self {
            graphics_prefix: format!("\x1b_Gi={image_id};").into_bytes(),
            ..Self::default()
        }
    }

    #[cfg(any(unix, test))]
    pub(crate) fn done(&self) -> bool {
        self.sentinel
    }

    #[cfg(any(unix, test))]
    pub(crate) fn supported(&self) -> bool {
        self.graphics == Some(true)
    }

    pub(crate) fn feed(&mut self, bytes: &[u8]) -> Vec<u8> {
        if self.graphics_prefix.is_empty() || (self.graphics.is_some() && self.sentinel) {
            return bytes.to_vec();
        }
        let mut input = Vec::with_capacity(bytes.len());
        for &byte in bytes {
            if self.pending.is_empty() {
                if byte != 0x1b {
                    input.push(byte);
                    continue;
                }
                self.held_since = Some(Instant::now());
            }
            self.pending.push(byte);
            loop {
                match self.matches() {
                    Match::Prefix => break,
                    Match::Graphics(supported) => {
                        self.graphics = Some(supported);
                        self.pending.clear();
                    }
                    Match::Sentinel => {
                        self.sentinel = true;
                        self.pending.clear();
                    }
                    Match::Unrelated => input.push(self.pending.remove(0)),
                }
                if self.pending.is_empty() {
                    self.held_since = None;
                    break;
                }
            }
        }
        input
    }

    pub(crate) fn holds_escape(&self) -> bool {
        !self.pending.is_empty()
            && !self.pending.starts_with(b"\x1b_G")
            && !self.pending.starts_with(b"\x1b[?")
    }

    /// Release an ambiguous bare Escape on the ordinary keyboard parser's timescale.
    ///
    /// Identifiable partial probe replies stay buffered across the deadline, so their trailing
    /// bytes cannot become pane keystrokes when the remote terminal eventually answers.
    pub(crate) fn expire(&mut self, now: Instant) -> Vec<u8> {
        if self.held_since.is_some_and(|since| {
            now.saturating_duration_since(since) >= crate::client_input::ESCAPE_DELAY
        }) && self.holds_escape()
        {
            self.held_since = None;
            return std::mem::take(&mut self.pending);
        }
        Vec::new()
    }

    fn matches(&self) -> Match {
        let bytes = self.pending.as_slice();
        if bytes.len() > REPLY_LIMIT {
            return Match::Unrelated;
        }
        if self.graphics.is_none() {
            if self.graphics_prefix.starts_with(bytes) {
                return Match::Prefix;
            }
            if let Some(body) = bytes.strip_prefix(self.graphics_prefix.as_slice()) {
                if let Some(message) = body.strip_suffix(b"\x1b\\") {
                    return Match::Graphics(message == b"OK");
                }
                let message = body.strip_suffix(b"\x1b").unwrap_or(body);
                if message.iter().all(|byte| (0x20..=0x7e).contains(byte)) {
                    return Match::Prefix;
                }
            }
        }
        if !self.sentinel {
            if b"\x1b[?".starts_with(bytes) {
                return Match::Prefix;
            }
            if let Some(parameters) = bytes.strip_prefix(b"\x1b[?") {
                let digits = parameters.strip_suffix(b"c").unwrap_or(parameters);
                if digits
                    .iter()
                    .all(|byte| byte.is_ascii_digit() || *byte == b';')
                {
                    return if parameters.ends_with(b"c") && digits.iter().any(u8::is_ascii_digit) {
                        Match::Sentinel
                    } else {
                        Match::Prefix
                    };
                }
            }
        }
        Match::Unrelated
    }
}

/// Separate the leading complete Kitty packets from the synchronized terminal repaint.
pub(crate) fn split_uploads(mut bytes: &[u8]) -> (&[u8], &[u8]) {
    let original = bytes;
    while bytes.starts_with(b"\x1b_G") {
        let Some(end) = bytes.windows(2).position(|pair| pair == b"\x1b\\") else {
            break;
        };
        bytes = &bytes[end + 2..];
    }
    original.split_at(original.len() - bytes.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_exact_known_hosts_and_vivido_candidates_are_selected() {
        for (term, vivid, expected) in [
            (Some("xterm-kitty"), false, Host::Known),
            (Some("xterm-ghostty"), false, Host::Known),
            (Some("vivido"), false, Host::Ask),
            (Some("xterm-256color"), true, Host::Ask),
            (None, true, Host::Ask),
            (Some("vvmux"), true, Host::No),
            (Some("vvmux"), false, Host::No),
            (Some("xterm"), false, Host::No),
            (Some("xterm-kitty-256color"), false, Host::No),
            (Some("ghostty"), false, Host::No),
            (None, false, Host::No),
        ] {
            assert_eq!(classify(term.map(OsStr::new), vivid), expected);
        }
    }

    #[test]
    fn query_has_no_quiet_flag_and_ends_with_the_sentinel_request() {
        assert_eq!(
            query(31),
            b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\x1b[c"
        );
    }

    #[test]
    fn replies_preserve_input_at_every_split_in_either_order() {
        for (graphics, supported) in [("OK", true), ("ENOTSUP: unavailable", false)] {
            for reverse in [false, true] {
                let graphics = format!("\x1b_Gi=31;{graphics}\x1b\\");
                let sentinel = "\x1b[?62;4;22c";
                let (first, second) = if reverse {
                    (sentinel, graphics.as_str())
                } else {
                    (graphics.as_str(), sentinel)
                };
                let bytes = format!("before{first}between{second}after").into_bytes();
                for split in 0..=bytes.len() {
                    let mut replies = Replies::new(31);
                    let mut input = replies.feed(&bytes[..split]);
                    input.extend(replies.feed(&bytes[split..]));
                    assert_eq!(input, b"beforebetweenafter", "split {split}");
                    assert!(replies.done());
                    assert_eq!(replies.supported(), supported);
                }
            }
        }
    }

    #[test]
    fn sentinel_only_finishes_without_support_and_other_ids_pass_through() {
        let mut replies = Replies::new(31);
        assert_eq!(
            replies.feed(b"\x1b_Gi=32;OK\x1b\\\x1b[?1;2c"),
            b"\x1b_Gi=32;OK\x1b\\"
        );
        assert!(replies.done());
        assert!(!replies.supported());
    }

    #[test]
    fn late_replies_are_removed_once_including_a_partial_reply_at_the_deadline() {
        for split in 3..b"\x1b_Gi=31;OK\x1b\\".len() {
            let mut replies = Replies::new(31);
            let graphics = b"\x1b_Gi=31;OK\x1b\\";
            assert!(replies.feed(&graphics[..split]).is_empty());
            assert!(replies.expire(Instant::now() + PROBE_TIMEOUT).is_empty());
            assert_eq!(replies.feed(&graphics[split..]), b"");
            assert_eq!(replies.feed(b"typed\x1b[?1;2c\x1b[A"), b"typed\x1b[A");
            assert_eq!(replies.feed(graphics), graphics);
            assert_eq!(replies.feed(b"\x1b[?1;2c"), b"\x1b[?1;2c");
        }
    }

    #[test]
    fn late_sentinel_is_removed_once_at_every_split() {
        let sentinel = b"\x1b[?62;4c";
        for split in 0..=sentinel.len() {
            let mut replies = Replies::new(31);
            let mut input = replies.feed(&sentinel[..split]);
            input.extend(replies.feed(&sentinel[split..]));
            assert!(input.is_empty());
            assert_eq!(replies.feed(sentinel), sentinel);
            assert_eq!(replies.feed(b"\x1b[A\x1b[Btyped"), b"\x1b[A\x1b[Btyped");
        }
    }

    #[test]
    fn uploads_larger_than_the_host_update_buffer_still_split_before_repaint() {
        // Vivido's synchronized-update buffer is 2 MiB; the graphics run exceeds it here.
        let mut frame = b"\x1b_G".to_vec();
        frame.resize(3 * 1024 * 1024, b'A');
        frame.extend_from_slice(b"\x1b\\repaint");
        let (upload, repaint) = split_uploads(&frame);
        assert_eq!(upload.len(), 3 * 1024 * 1024 + 2);
        assert_eq!(repaint, b"repaint");
    }

    #[test]
    fn escape_input_is_released_and_malformed_candidates_are_bounded() {
        let mut replies = Replies::new(31);
        assert!(replies.feed(b"\x1b").is_empty());
        assert_eq!(replies.expire(Instant::now() + PROBE_TIMEOUT), b"\x1b");
        let mut malformed = b"\x1b_Gi=31;".to_vec();
        malformed.extend(vec![b'x'; REPLY_LIMIT * 2]);
        assert_eq!(replies.feed(&malformed), malformed);
        assert!(replies.pending.len() <= REPLY_LIMIT);
    }

    #[test]
    fn upload_split_requires_complete_leading_packets() {
        let bytes = b"\x1b_Gfirst\x1b\\\x1b_Gsecond\x1b\\repaint\x1b_Glater\x1b\\";
        assert_eq!(split_uploads(bytes), (&bytes[..21], &bytes[21..]));
        assert_eq!(split_uploads(b"repaint"), (&b""[..], &b"repaint"[..]));
        assert_eq!(
            split_uploads(b"\x1b_Gpartial"),
            (&b""[..], &b"\x1b_Gpartial"[..])
        );
    }
}
