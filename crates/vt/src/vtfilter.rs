//! What remote output may reach the local terminal, and
//! printing remote strings safely.

/// Decides what remote output reaches the local terminal, which is not the
/// remote program's to command. Plain text and CSI pass, minus queries the
/// holder's terminal already answered (their local replies would arrive as
/// typed input: DSR, DA, DECRQM, XTVERSION, kitty keyboard). String
/// sequences are allowlisted: OSC 0/1/2 (titles, control characters
/// stripped), 7 (cwd), 8 (hyperlinks) and 133 (prompt marks) pass; every
/// other OSC (clipboard, notifications, color queries, file transfer), DCS
/// (including tmux and screen passthrough), APC, PM and SOS is dropped.
/// With graphics on, Kitty graphics commands (APC G) pass, made quiet (q=2)
/// since the holder already answered them; queries (a=q) and transmissions
/// the local terminal would read from its own disk or memory (t=f, t=t,
/// t=s) are dropped.
/// UTF-8 encoded C1 controls (U+0080-U+009F) are dropped: libghostty doesn't
/// act on them, so the mirror has none, and a local terminal that would
/// must not.
///
/// State carries across filter calls, so a sequence split between OUTPUT
/// frames is handled the same as a whole one. It also holds back a UTF-8
/// character split between frames, so whatever is drawn between frames
/// (predictions, notices) never lands inside one, and it tracks the current
/// SGR style so such drawing can restore it.
#[derive(Default, Clone)]
pub struct QueryFilter {
    pub state: FState,
    /// The sequence so far, while it is being decided.
    buf: Vec<u8>,
    /// The string sequence's introducer: ] P _ ^ X
    kind: u8,
    /// Within a long string sequence: pass streams it out, drop discards it.
    pass: bool,
    drop: bool,
    /// The previous byte in a string was ESC.
    str_esc: bool,
    /// A UTF-8 character still missing bytes.
    pub rune: Vec<u8>,
    /// Every SGR sequence since the last reset: replayed after "\x1b[0m", it
    /// restores the remote's current style.
    sgr: Vec<u8>,
    /// Passes Kitty graphics commands.
    pub graphics: bool,
    /// A graphics command ended, passed or not, since take_graphics.
    saw_graphics: bool,
    /// The string being read is a graphics command.
    in_graphics: bool,
    /// The next string byte is an APC's first.
    apc_start: bool,
    /// Images whose pixels passed in full since take_graphics.
    sent_images: Vec<u32>,
    /// The image a passed chunked transmission (m=1) is loading.
    loading: Option<u32>,
    /// The rest of a dropped chunked transmission is dropped too.
    drop_chunks: bool,
    /// A color set or reset (OSC 4, 10-12, 21, 104, 110-112) passed since
    /// the UI last cleared it.
    pub passed_colors: bool,
}

/// Bounds the tracked style; a program that never resets loses exactness
/// here, not correctness elsewhere.
const SGR_MAX: usize = 512;

#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
pub enum FState {
    #[default]
    Ground,
    Esc,
    Csi,
    /// A CSI too long to hold: dropped through its final byte.
    CsiSkip,
    Str,
}

const ESC: u8 = 0x1b;
const CAN: u8 = 0x18;
const SUB: u8 = 0x1a;
const BEL: u8 = 0x07;
/// Past CSI_MAX a CSI is garbage and is dropped; past STR_MAX an OSC is
/// decided on its number and streamed or dropped (a title that long is
/// dropped).
const CSI_MAX: usize = 256;
const STR_MAX: usize = 4096;

const REPLACEMENT: &[u8] = "\u{FFFD}".as_bytes();

/// The length of the UTF-8 sequence a lead byte starts, 0 for a byte that
/// doesn't start one.
fn utf8_len(b: u8) -> usize {
    match b {
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => 0,
    }
}

/// Whether a held lead byte and continuation bytes are a full character:
/// it is complete, or already can't be one.
fn full_rune(b: &[u8]) -> bool {
    if b.len() >= utf8_len(b[0]) {
        return true;
    }
    let (lo, hi) = match b[0] {
        0xe0 => (0xa0, 0xbf),
        0xed => (0x80, 0x9f),
        0xf0 => (0x90, 0xbf),
        0xf4 => (0x80, 0x8f),
        _ => (0x80, 0xbf),
    };
    b.len() >= 2 && !(lo..=hi).contains(&b[1])
}

impl QueryFilter {
    /// Returns input without the filtered sequences.
    pub fn filter(&mut self, input: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(input.len());
        for &b in input {
            self.c1(b, &mut out);
        }
        out
    }

    /// Forgets a partial sequence, for when the stream restarts at a new
    /// snapshot, and takes style as the current SGR state (from the redraw).
    pub fn reset(&mut self, style: &[u8]) {
        let mut buf = std::mem::take(&mut self.buf);
        buf.clear();
        *self = QueryFilter { buf, sgr: style.to_vec(), graphics: self.graphics, passed_colors: self.passed_colors, ..Default::default() };
    }

    /// Whether a graphics command went by since the last call, and the
    /// images whose pixels passed.
    pub fn take_graphics(&mut self) -> (bool, Vec<u32>) {
        (std::mem::take(&mut self.saw_graphics), std::mem::take(&mut self.sent_images))
    }

    /// Sequences that set the remote's current style.
    pub fn style(&self) -> Vec<u8> {
        let mut s = b"\x1b[0m".to_vec();
        s.extend_from_slice(&self.sgr);
        s
    }

    /// Assembles UTF-8 characters before anything else sees them, in every
    /// state: a C1 control (U+0080-U+009F) is dropped whole, and a character
    /// split between frames is held back until it is complete. Bytes that
    /// aren't valid UTF-8 become U+FFFD, as the mirror shows them: passed on
    /// raw, a broken byte and a later one could make a C1 control together.
    fn c1(&mut self, b: u8, out: &mut Vec<u8>) {
        if !self.rune.is_empty() {
            if b & 0xc0 == 0x80 {
                self.rune.push(b);
                if !full_rune(&self.rune) {
                    return;
                }
                let held = std::mem::take(&mut self.rune);
                match std::str::from_utf8(&held).ok().and_then(|s| s.chars().next()) {
                    Some(r) if ('\u{80}'..='\u{9f}').contains(&r) => {}
                    None => self.feed(REPLACEMENT, out),
                    Some(_) => self.feed(&held, out),
                }
                return;
            }
            // Not a continuation: the lead byte was broken.
            self.rune.clear();
            self.feed(REPLACEMENT, out);
        }
        if b < 0x80 {
            return self.byte(b, out);
        }
        if b >= 0xc0 && utf8_len(b) > 1 {
            self.rune.push(b);
            return;
        }
        // A continuation byte with no lead, or a byte that never starts one.
        self.feed(REPLACEMENT, out)
    }

    fn feed(&mut self, b: &[u8], out: &mut Vec<u8>) {
        for &x in b {
            self.byte(x, out);
        }
    }

    fn byte(&mut self, b: u8, out: &mut Vec<u8>) {
        match self.state {
            FState::Ground => {
                if b == ESC {
                    self.state = FState::Esc;
                    self.buf.clear();
                    self.buf.push(b);
                    return;
                }
                out.push(b);
            }
            FState::Esc => {
                self.buf.push(b);
                match b {
                    b'[' => self.state = FState::Csi,
                    b']' | b'P' | b'_' | b'^' | b'X' => {
                        self.state = FState::Str;
                        self.kind = b;
                        // Only OSC and graphics APC can be allowed; the rest
                        // go unseen.
                        self.drop = b != b']' && !(b == b'_' && self.graphics);
                        self.apc_start = b == b'_';
                    }
                    // ESC ESC: the first one is ignored. Passing it alone
                    // would let whatever passes next complete a sequence we
                    // dropped.
                    ESC => self.buf.truncate(1),
                    _ => {
                        out.extend_from_slice(&self.buf);
                        self.state = FState::Ground;
                    }
                }
            }
            FState::Csi | FState::CsiSkip => {
                // An aborted or cancelled CSI does nothing, and is dropped: a
                // prefix passed on could be completed by what passes next.
                if b == ESC {
                    self.state = FState::Esc;
                    self.buf.clear();
                    self.buf.push(b);
                    return;
                }
                if b == CAN || b == SUB {
                    self.state = FState::Ground;
                    out.push(b);
                    return;
                }
                if self.state == FState::CsiSkip {
                    if (0x40..=0x7e).contains(&b) {
                        self.state = FState::Ground;
                    }
                    return;
                }
                self.buf.push(b);
                if (0x40..=0x7e).contains(&b) {
                    if !csi_query(&self.buf) {
                        out.extend_from_slice(&without_own_modes(&self.buf));
                    }
                    if b == b'm' {
                        let seq = std::mem::take(&mut self.buf);
                        self.track_sgr(&seq);
                        self.buf = seq;
                    }
                    self.state = FState::Ground;
                } else if self.buf.len() > CSI_MAX {
                    self.state = FState::CsiSkip;
                }
            }
            FState::Str => self.str(b, out),
        }
    }

    /// A byte inside a string sequence (OSC, DCS, APC, PM, SOS).
    fn str(&mut self, b: u8, out: &mut Vec<u8>) {
        if self.str_esc {
            self.str_esc = false;
            if b == b'\\' {
                return self.end_str(out, b"\x1b\\");
            }
            if self.drop && self.kind != b']' {
                // Inside DCS, APC, PM and SOS everything up to ST goes: tmux
                // passthrough doubles each ESC of what it wraps.
                return;
            }
            // ESC aborts the string, which has no effect, and starts a new
            // sequence.
            self.end_abort();
            self.state = FState::Esc;
            self.buf.clear();
            self.buf.push(ESC);
            return self.byte(b, out);
        }
        if b == ESC {
            self.str_esc = true;
            return;
        }
        // Flagged when the command ends, once the mirror has applied it.
        if std::mem::take(&mut self.apc_start) && b == b'G' {
            self.in_graphics = true;
        }
        if b == BEL && self.kind == b']' {
            return self.end_str(out, b"\x07");
        }
        if b == CAN || b == SUB {
            self.end_abort();
            out.push(b);
            return;
        }
        if self.drop {
            return;
        }
        if self.pass {
            out.push(b);
            return;
        }
        self.buf.push(b);
        if self.kind == b'_' {
            // ESC _ G control-keys ; payload: decided once the keys are in.
            if self.buf.len() == 3 && b != b'G' || self.buf.len() > CSI_MAX {
                self.drop = true;
                self.buf.clear();
            } else if b == b';' {
                self.graphics_cmd(out, true);
            }
            return;
        }
        if self.buf.len() > STR_MAX {
            match osc_number(&self.buf[2..]) {
                7 | 8 | 133 => {
                    self.pass = true;
                    out.extend_from_slice(&self.buf);
                }
                _ => self.drop = true,
            }
            self.buf.clear();
        }
    }

    fn end_abort(&mut self) {
        self.in_graphics = false;
        self.pass = false;
        self.drop = false;
        self.state = FState::Ground;
        self.buf.clear();
    }

    /// Finishes a string sequence with terminator st, letting through what
    /// the allowlist allows.
    fn end_str(&mut self, out: &mut Vec<u8>, st: &[u8]) {
        self.saw_graphics |= self.in_graphics;
        if self.drop {
        } else if self.pass {
            out.extend_from_slice(st);
        } else if self.kind == b'_' {
            if self.buf.len() >= 3 && self.graphics_cmd(out, false) {
                out.extend_from_slice(st);
            }
        } else {
            let body = &self.buf[2..];
            match osc_number(body) {
                n @ (0..=2) => {
                    // A title without text means nothing; drop it.
                    if let Some(i) = body.iter().position(|&c| c == b';') {
                        out.extend_from_slice(format!("\x1b]{n};").as_bytes());
                        out.extend_from_slice(&strip_controls(&body[i + 1..]));
                        out.extend_from_slice(st);
                    }
                }
                7 | 8 | 133 => {
                    out.extend_from_slice(&self.buf);
                    out.extend_from_slice(st);
                }
                n @ (4 | 10..=12 | 21 | 104 | 110..=112) => {
                    let sets = color_sets(n, body, st);
                    self.passed_colors |= !sets.is_empty();
                    out.extend_from_slice(&sets);
                }
                _ => {}
            }
        }
        self.end_abort();
    }

    /// Decides a graphics command whose control keys are in buf (with the
    /// ';' when a payload follows): passes it quiet, its payload streaming
    /// after, or drops it whole.
    fn graphics_cmd(&mut self, out: &mut Vec<u8>, payload: bool) -> bool {
        let end = self.buf.len() - payload as usize;
        let keys: Vec<(u8, &[u8])> = self.buf[3..end]
            .split(|&c| c == b',')
            .filter_map(|kv| match kv {
                [k, b'=', v @ ..] => Some((*k, v)),
                _ => None,
            })
            .collect();
        let get = |k: u8| keys.iter().find(|(key, _)| *key == k).map(|(_, v)| *v);
        let more = get(b'm') == Some(b"1");
        // Kitty takes any command during a chunked transmission as its next
        // chunk.
        if self.drop_chunks {
            self.drop_chunks = more;
            self.drop = true;
            return false;
        }
        if let Some(id) = self.loading {
            if !more {
                self.loading = None;
                self.sent_images.push(id);
            }
        } else {
            let action = get(b'a').unwrap_or(b"t");
            if action == b"q" || get(b't').is_some_and(|t| t != b"d") {
                self.drop_chunks = more;
                self.drop = true;
                return false;
            }
            let id = get(b'i').and_then(|v| std::str::from_utf8(v).ok()?.parse::<u32>().ok());
            if let (Some(id), b"t" | b"T" | b"f") = (id, action) {
                if more {
                    self.loading = Some(id);
                } else {
                    self.sent_images.push(id);
                }
            }
        }
        out.extend_from_slice(b"\x1b_G");
        for (k, v) in keys.iter().filter(|(k, _)| *k != b'q') {
            out.extend_from_slice(&[*k, b'=']);
            out.extend_from_slice(v);
            out.push(b',');
        }
        out.extend_from_slice(b"q=2");
        if payload {
            out.push(b';');
            self.pass = true;
        }
        self.buf.clear();
        true
    }

    /// Follows the style set by a complete CSI ... m.
    fn track_sgr(&mut self, seq: &[u8]) {
        let params = &seq[2..seq.len() - 1];
        // Private or intermediate: not plain SGR.
        if params.iter().any(|&c| !(b'0'..=b';').contains(&c)) {
            return;
        }
        if params.is_empty() || params == b"0" || params.starts_with(b"0;") {
            self.sgr.clear();
            if params.len() <= 1 {
                return;
            }
        }
        if self.sgr.len() + seq.len() > SGR_MAX {
            self.sgr.clear();
        }
        self.sgr.extend_from_slice(seq);
    }
}

fn is_control(c: char) -> bool {
    c.is_control()
}

/// Removes control characters (C0, DEL, C1) from text. Bytes that aren't
/// UTF-8 stay as they are, as the terminal would see them.
pub fn strip_controls(mut text: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    while !text.is_empty() {
        let (n, ch) = decode_rune(text);
        match ch {
            Some(c) if is_control(c) => {}
            _ => out.extend_from_slice(&text[..n]),
        }
        text = &text[n..];
    }
    out
}

/// Decodes one UTF-8 character: its length, and the
/// character, or None for an invalid byte (length 1).
fn decode_rune(b: &[u8]) -> (usize, Option<char>) {
    let n = utf8_len(b[0]).max(1);
    if b[0] < 0x80 {
        return (1, Some(b[0] as char));
    }
    if n > 1 && b.len() >= n {
        if let Ok(s) = std::str::from_utf8(&b[..n]) {
            return (n, s.chars().next());
        }
    }
    (1, None)
}

/// printable line by line, keeping the line breaks.
pub fn printable_lines(s: &str) -> String {
    s.split('\n').map(printable).collect::<Vec<_>>().join("\n")
}

/// Replaces control characters in a remote string (a title, a client id, an
/// error message) before it is printed, so it can't drive the local
/// terminal.
pub fn printable(s: &str) -> String {
    s.chars().map(|c| if is_control(c) { '?' } else { c }).collect()
}

/// Reports whether a complete CSI sequence asks the terminal for a reply.
fn csi_query(seq: &[u8]) -> bool {
    let (body, fin) = (&seq[2..seq.len() - 1], seq[seq.len() - 1]);
    let prefix = match body.first() {
        Some(&c) if (b'<'..=b'?').contains(&c) => c,
        _ => 0,
    };
    let inter: Vec<u8> = body.iter().copied().filter(|c| (0x20..=0x2f).contains(c)).collect();
    match fin {
        // DSR and DEC DSR: status, cursor position
        b'n' if inter.is_empty() => true,
        // DA1, DA2, DA3
        b'c' if inter.is_empty() && matches!(prefix, 0 | b'>' | b'=') => true,
        // DECRQM, ANSI and DEC
        b'p' if inter == b"$" => true,
        // XTVERSION
        b'q' if prefix == b'>' && inter.is_empty() => true,
        // kitty keyboard flags query
        b'u' if prefix == b'?' && inter.is_empty() => true,
        // XTWINOPS reports: state, position, sizes, title. The holder
        // answers the sizes; the rest would tell the program about this
        // window, or type its title.
        b't' if prefix == 0 && inter.is_empty() => {
            let first = body.split(|&c| c == b';').next().unwrap_or_default();
            matches!(first, b"11" | b"13" | b"14" | b"15" | b"16" | b"18" | b"19" | b"20" | b"21")
        }
        _ => false,
    }
}

/// A color OSC's sets and resets, without its queries, which the holder
/// answers.
fn color_sets(n: i64, body: &[u8], st: &[u8]) -> Vec<u8> {
    let params: Vec<&[u8]> = body.split(|&c| c == b';').skip(1).collect();
    let osc = |n: i64, rest: &[u8]| {
        let mut o = format!("\x1b]{n}").into_bytes();
        if !rest.is_empty() {
            o.push(b';');
            o.extend_from_slice(rest);
        }
        o.extend_from_slice(st);
        o
    };
    match n {
        // 4;index;spec;index;spec...
        4 => {
            let sets: Vec<&[u8]> = params.chunks(2).filter(|p| p.len() == 2 && p[1] != b"?").flat_map(|p| [p[0], p[1]]).collect();
            if sets.is_empty() {
                return Vec::new();
            }
            osc(4, &sets.join(&b';'))
        }
        // Each spec is for the next color on: 10;fg;bg sets both.
        10..=12 => {
            let mut out = Vec::new();
            for (target, spec) in (n..=12).zip(params) {
                if spec != b"?" {
                    out.extend_from_slice(&osc(target, spec));
                }
            }
            out
        }
        // key=value;key=?...
        21 => {
            let sets: Vec<&[u8]> = params.into_iter().filter(|p| !p.ends_with(b"=?")).collect();
            if sets.is_empty() {
                return Vec::new();
            }
            osc(21, &sets.join(&b';'))
        }
        _ => osc(n, &body[body.iter().position(|&c| c == b';').map_or(body.len(), |i| i + 1)..]),
    }
}

/// A DEC mode set or reset (CSI ? Pm h/l) without the modes that aren't
/// the local terminal's to follow: color scheme reports (2031), which the
/// holder answers for the program and the client needs on to learn of
/// changes, and Kitty paste events (5522), whose reads only the holder can
/// answer. Empty when none are left.
fn without_own_modes(seq: &[u8]) -> std::borrow::Cow<'_, [u8]> {
    let fin = seq[seq.len() - 1];
    if seq.len() < 4 || seq[2] != b'?' || !matches!(fin, b'h' | b'l') {
        return seq.into();
    }
    let params = &seq[3..seq.len() - 1];
    let own = |p: &[u8]| p == b"2031" || p == b"5522";
    if !params.split(|&c| c == b';').any(own) {
        return seq.into();
    }
    let kept: Vec<&[u8]> = params.split(|&c| c == b';').filter(|p| !own(p)).collect();
    if kept.is_empty() {
        return Vec::new().into();
    }
    let mut out = b"\x1b[?".to_vec();
    out.extend_from_slice(&kept.join(&b';'));
    out.push(fin);
    out.into()
}

/// Parses an OSC's leading number from its body, or -1.
fn osc_number(body: &[u8]) -> i64 {
    let end = body.iter().position(|&c| c == b';').unwrap_or(body.len());
    // A plain integer: an optional sign, then digits.
    let s = std::str::from_utf8(&body[..end]).unwrap_or("");
    s.parse::<i64>().unwrap_or(-1)
}
