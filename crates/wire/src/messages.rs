use serde::{Deserialize, Serialize};

fn is_zero_i64(v: &i64) -> bool {
    *v == 0
}
fn is_zero_u32(v: &u32) -> bool {
    *v == 0
}
fn is_false(v: &bool) -> bool {
    !*v
}

// v0 peers may send an empty list as null; serde rejects null for a Vec,
// so every list a peer may send accepts it.
fn null_vec<'de, D, T>(d: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Option::<Vec<T>>::deserialize(d)?.unwrap_or_default())
}

/// The client's first frame.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct Hello {
    #[serde(default)]
    pub version: i32,
    #[serde(default)]
    pub client_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// Features the client understands. The server sends nothing that
    /// belongs to a feature the client didn't list.
    #[serde(default, deserialize_with = "null_vec", skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<String>,
}

/// The server's first frame.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct Welcome {
    #[serde(default)]
    pub version: i32,
    #[serde(default)]
    pub server: String,
    #[serde(default)]
    pub snapshot_version: i32,
    #[serde(default)]
    pub ghostty: String,
    #[serde(default, deserialize_with = "null_vec")]
    pub carriers: Vec<Carrier>,
    /// Features the server offers. A client uses a feature only when both
    /// lists have it.
    #[serde(default, deserialize_with = "null_vec", skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<String>,
}

pub const FEATURE_FORWARD: &str = "forward";
pub const FEATURE_EXEC: &str = "exec";

/// Reports whether list has f.
pub fn has_feature(list: &[String], f: &str) -> bool {
    list.iter().any(|x| x == f)
}

/// Another way to reach the same server.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct Carrier {
    #[serde(default)]
    pub carrier: String,
    #[serde(default)]
    pub port: i64,
    #[serde(default, deserialize_with = "null_vec", skip_serializing_if = "Vec::is_empty")]
    pub alpn: Vec<String>,
}

pub const METHOD_LIST: &str = "session.list";
pub const METHOD_CREATE: &str = "session.create";
pub const METHOD_ATTACH: &str = "session.attach";
pub const METHOD_DETACH: &str = "session.detach";
pub const METHOD_OWN: &str = "session.own";
pub const METHOD_RESIZE: &str = "session.resize";
pub const METHOD_COLOR_SCHEME: &str = "session.color_scheme";
pub const METHOD_COLORS: &str = "session.colors";
pub const METHOD_CLIPBOARD: &str = "session.clipboard";
pub const METHOD_KILL: &str = "session.kill";
pub const METHOD_FORWARD_OPEN: &str = "forward.open";
pub const METHOD_EXEC_START: &str = "exec.start";
pub const METHOD_STREAM_CLOSE: &str = "stream.close";
/// Holder-only: the server asks each holder for its info.
pub const METHOD_INFO: &str = "session.info";

/// A REQUEST payload. Only the fields its method uses are set.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct Request {
    #[serde(default)]
    pub id: u64,
    #[serde(default)]
    pub method: String,

    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub session: String,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub stream: u32,
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub cols: i64,
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub rows: i64,
    /// The text area in pixels, with cols and rows; 0 when unknown.
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub width: i64,
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub height: i64,
    #[serde(default, skip_serializing_if = "is_false")]
    pub own: bool,
    /// session.color_scheme: "light" or "dark"
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub color_scheme: String,
    /// session.colors: "#rrggbb", or empty when unknown; the palette has
    /// 256 entries or none.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub foreground: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub background: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub cursor: String,
    #[serde(default, deserialize_with = "null_vec", skip_serializing_if = "Vec::is_empty")]
    pub palette: Vec<String>,
    /// session.clipboard: what the clipboard holds, base64 in JSON
    #[serde(default, with = "base64_bytes", skip_serializing_if = "Vec::is_empty")]
    pub data: Vec<u8>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub epoch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<u64>,
    #[serde(default, deserialize_with = "null_vec", skip_serializing_if = "Vec::is_empty")]
    pub argv: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub cwd: String,
    #[serde(default, deserialize_with = "null_vec", skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<String>,
    // forward.open
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub host: String,
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub port: i64,

    /// Set by the server when it forwards a request to a holder; clients
    /// leave it empty.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub client_id: String,
}

pub const ERR_BAD_REQUEST: &str = "bad_request";
pub const ERR_NOT_FOUND: &str = "not_found";
pub const ERR_NOT_OWNER: &str = "not_owner";
pub const ERR_ENDED: &str = "ended";
pub const ERR_UNAVAILABLE: &str = "unavailable";
pub const ERR_INTERNAL: &str = "internal";
/// forward.open could not connect.
pub const ERR_UNREACHABLE: &str = "unreachable";

/// A RESPONSE payload.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct Response {
    #[serde(default)]
    pub id: u64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,

    // session.list
    #[serde(default, deserialize_with = "null_vec", skip_serializing_if = "Vec::is_empty")]
    pub sessions: Vec<SessionInfo>,
    // session.create
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub session_id: String,
    /// session.attach. Head is the log end when the holder took the attach:
    /// records with end <= head are catch-up, and never fire effects.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub epoch: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub resumed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<u64>,
    // session.attach, session.own
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub owner: String,
}

impl Response {
    pub fn error(id: u64, code: &str, message: impl Into<String>) -> Response {
        Response { id, error: code.into(), message: message.into(), ..Default::default() }
    }
}

/// One session in session.list.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct SessionInfo {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub cols: i64,
    #[serde(default)]
    pub rows: i64,
    #[serde(default)]
    pub created: i64,
    #[serde(default)]
    pub exited: bool,
    #[serde(default)]
    pub exit_code: i64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub owner: String,
    /// The number of attached streams.
    #[serde(default)]
    pub attached: i64,
    /// How long the session has had no attached stream and no PTY output.
    #[serde(default)]
    pub idle_seconds: i64,
    /// The latest root OSC 7501 record, if the program reports one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ProgramStatus>,
    /// The holder's build, which encodes the session's snapshots. A
    /// holder outlives the server that started it, so it may be another
    /// build than the server now serving it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<Build>,
    /// Set by the server when the holder's build isn't its own: why it
    /// won't attach the session, which can still be listed and killed.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub incompatible: String,
}

/// A build as far as snapshots go: the fields of WELCOME, and of
/// `osewa version --json`, that must be equal for two ends to interoperate.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct Build {
    #[serde(default)]
    pub version: i32,
    #[serde(default)]
    pub ghostty: String,
    #[serde(default)]
    pub snapshot_version: i32,
}

impl Build {
    /// `p<version>-g<ghostty, 12>-s<snapshot_version>`, as releases name it.
    pub fn id(&self) -> String {
        format!("p{}-g{}-s{}", self.version, self.ghostty.get(..12).unwrap_or(&self.ghostty), self.snapshot_version)
    }
}

/// One OSC 7501 program status record.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct ProgramStatus {
    /// idle, working, done, blocked, error
    #[serde(default)]
    pub state: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub app: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub msg: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<i64>,
}

/// Starts a snapshot on a stream.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct SnapshotBegin {
    #[serde(default)]
    pub generation: u32,
    #[serde(default)]
    pub epoch: String,
    #[serde(default)]
    pub position: u64,
    #[serde(default)]
    pub cols: i64,
    #[serde(default)]
    pub rows: i64,
    /// False for a screen-only catch-up snapshot; None means true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history: Option<bool>,
}

/// Closes a stream.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct StreamEnd {
    #[serde(default)]
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
}

pub const END_DETACHED: &str = "detached";
pub const END_EXITED: &str = "exited";
pub const END_KILLED: &str = "killed";
pub const END_ERROR: &str = "error";
/// A channel stream closed by stream.close, or a forward whose connection
/// ended both ways.
pub const END_CLOSED: &str = "closed";

/// An INPUT payload.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct Input {
    #[serde(default)]
    pub seq: u64,
    /// key, paste, text, focus
    #[serde(default, rename = "type")]
    pub ty: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub key: String,
    #[serde(default, deserialize_with = "null_vec", skip_serializing_if = "Vec::is_empty")]
    pub mods: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    /// press, repeat, release
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub action: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub focused: bool,
}

pub const EVENT_RESIZE: &str = "resize";
pub const EVENT_BELL: &str = "bell";
pub const EVENT_CLIPBOARD: &str = "clipboard";
pub const EVENT_TITLE: &str = "title";
pub const EVENT_STATUS: &str = "status";
pub const EVENT_OWNER: &str = "owner";
pub const EVENT_EXIT: &str = "exit";
pub const EVENT_INPUT_MODE: &str = "input_mode";
pub const EVENT_NOTIFY: &str = "notify";
pub const EVENT_CLIPBOARD_READ: &str = "clipboard_read";

/// An EVENT's JSON body. Only the fields its kind uses are set.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct EventMsg {
    #[serde(default)]
    pub kind: String,
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub cols: i64,
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub rows: i64,
    /// resize: the text area in pixels, 0 when unknown
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub width: i64,
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub height: i64,
    /// clipboard, base64 in JSON
    #[serde(default, with = "base64_bytes", skip_serializing_if = "Vec::is_empty")]
    pub data: Vec<u8>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
    /// clipboard_read: c, p or s, as OSC 52 names them
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub selection: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub state: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub app: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub msg: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub owner: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<i64>,
    // input_mode
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub echo: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canonical: Option<bool>,
}

impl EventMsg {
    pub fn kind(kind: &str) -> EventMsg {
        EventMsg { kind: kind.into(), ..Default::default() }
    }
}

/// A color as session.colors has it: "#rrggbb".
pub fn rgb_hex([r, g, b]: [u8; 3]) -> String {
    format!("#{r:02x}{g:02x}{b:02x}")
}

/// Parses "#rrggbb"; None for anything else.
pub fn parse_rgb_hex(s: &str) -> Option<[u8; 3]> {
    let h = s.strip_prefix('#')?;
    if h.len() != 6 || !h.bytes().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let v = u32::from_str_radix(h, 16).ok()?;
    Some([(v >> 16) as u8, (v >> 8) as u8, v as u8])
}

/// Encodes an event body.
pub fn marshal_event(e: &EventMsg) -> Vec<u8> {
    crate::marshal(e)
}

/// Bytes in JSON as v0 has them: standard base64 with padding, null for
/// none.
mod base64_bytes {
    use base64::Engine;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(b: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&base64::engine::general_purpose::STANDARD.encode(b))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        match Option::<String>::deserialize(d)? {
            None => Ok(Vec::new()),
            Some(s) => base64::engine::general_purpose::STANDARD.decode(s).map_err(serde::de::Error::custom),
        }
    }
}
