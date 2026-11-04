//! osewa_open_options read into Rust: everything checked up front, so a
//! bad option fails osewa_open rather than a connection later.

use std::ffi::{c_char, CStr};
use std::sync::Arc;

use osewa_client::TermColors;
use osewa_mirror::PredictMode;
use osewa_wire as wire;

/// The WebSocket/TLS port when the address names none.
pub const DEFAULT_PORT: u16 = 7322;

/// osewa_open_options, as osewa.h lays it out.
#[repr(C)]
pub struct OpenOptions {
    pub addr: *const c_char,
    pub server_key: *const c_char,
    pub quic_port: u16,
    pub device_key: *const u8,
    pub device_key_len: usize,
    pub client_id: *const c_char,
    pub device_name: *const c_char,
    pub session_id: *const c_char,
    pub argv_json: *const c_char,
    pub cwd: *const c_char,
    pub cols: u16,
    pub rows: u16,
    pub width_px: u32,
    pub height_px: u32,
    pub predict: i32,
    pub appearance_json: *const c_char,
    pub tag: *const c_char,
}

/// What a session is opened with.
pub struct Opened {
    pub cfg: osewa_client::Config,
    pub session_id: Option<String>,
    pub argv: Vec<String>,
    pub cwd: String,
    pub cols: u16,
    pub rows: u16,
    pub width: u32,
    pub height: u32,
    pub predict: PredictMode,
    pub appearance: Appearance,
    pub tag: String,
}

/// The app terminal's look, told to the holder for programs that ask.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Appearance {
    pub dark: Option<bool>,
    pub colors: TermColors,
}

/// A C string option: None for NULL or empty.
///
/// # Safety
/// p is NULL or a NUL-terminated string.
unsafe fn opt_str(p: *const c_char, what: &str) -> Result<Option<String>, String> {
    if p.is_null() {
        return Ok(None);
    }
    let s = unsafe { CStr::from_ptr(p) }.to_str().map_err(|_| format!("osewa: {what} is not UTF-8"))?;
    Ok((!s.is_empty()).then(|| s.to_string()))
}

/// Reads and checks the options.
///
/// # Safety
/// Every pointer in o is NULL or valid as osewa.h says.
pub unsafe fn read(o: &OpenOptions) -> Result<Opened, String> {
    unsafe { read_session(o, true) }
}

/// Reads and checks the options of a session on an osewa_link, which has
/// the connection: addr, server_key, quic_port, device_key, client_id and
/// device_name are ignored.
///
/// # Safety
/// Every pointer in o is NULL or valid as osewa.h says.
pub unsafe fn read_on(o: &OpenOptions) -> Result<Opened, String> {
    unsafe { read_session(o, false) }
}

unsafe fn read_session(o: &OpenOptions, connect: bool) -> Result<Opened, String> {
    let session_id = unsafe { opt_str(o.session_id, "session_id") }?;
    let cwd = unsafe { opt_str(o.cwd, "cwd") }?.unwrap_or_default();
    let tag = unsafe { opt_str(o.tag, "tag") }?.unwrap_or_default();
    if tag.len() > 128 || tag.chars().any(char::is_control) {
        return Err("osewa: tag must be printable and at most 128 bytes".into());
    }
    let argv = match unsafe { opt_str(o.argv_json, "argv_json") }? {
        None => Vec::new(),
        Some(j) => serde_json::from_str::<Vec<String>>(&j).map_err(|e| format!("osewa: argv_json: {e}"))?,
    };
    let appearance = match unsafe { opt_str(o.appearance_json, "appearance_json") }? {
        None => Appearance::default(),
        Some(j) => parse_appearance(&j)?,
    };
    let cfg = if connect {
        let addr = unsafe { opt_str(o.addr, "addr") }?.ok_or("osewa: addr is required")?;
        let server_key = unsafe { opt_str(o.server_key, "server_key") }?.ok_or("osewa: server_key is required")?;
        let client_id = unsafe { opt_str(o.client_id, "client_id") }?.ok_or("osewa: client_id is required")?;
        let name = unsafe { opt_str(o.device_name, "device_name") }?.unwrap_or_default();
        unsafe { config(&addr, &server_key, o.quic_port, o.device_key, o.device_key_len, client_id, name) }?
    } else {
        osewa_client::Config::default()
    };
    Ok(Opened {
        cfg,
        session_id,
        argv,
        cwd,
        cols: o.cols,
        rows: o.rows,
        width: o.width_px,
        height: o.height_px,
        predict: if o.predict != 0 { PredictMode::Auto } else { PredictMode::Never },
        appearance,
        tag,
    })
}

/// The connection's config: the server's URL, mutual TLS with the device
/// key and the pinned server key.
///
/// # Safety
/// device_key is NULL or holds device_key_len bytes.
unsafe fn config(
    addr: &str,
    server_key: &str,
    quic_port: u16,
    device_key: *const u8,
    device_key_len: usize,
    client_id: String,
    name: String,
) -> Result<osewa_client::Config, String> {
    if device_key.is_null() || device_key_len == 0 {
        return Err("osewa: device_key is required".into());
    }
    let der = unsafe { std::slice::from_raw_parts(device_key, device_key_len) }.to_vec();
    let id = osewa_auth::Identity::from_pkcs8(der).map_err(|e| format!("osewa: device_key: {e}"))?;
    let pin = osewa_auth::decode_key(server_key).map_err(|e| format!("osewa: server_key: {e}"))?;
    let mut cfg = osewa_client::Config::default();
    cfg.url = format!("wss://{}{}", with_port(addr)?, wire::PATH);
    cfg.quic_port = quic_port;
    cfg.tls = Some(Arc::new(osewa_auth::client_config(&id, pin)));
    cfg.client_id = client_id;
    cfg.name = name;
    Ok(cfg)
}

/// osewa_link_options, as osewa.h lays it out: osewa_open_options'
/// connection half.
#[repr(C)]
pub struct LinkOptions {
    pub addr: *const c_char,
    pub server_key: *const c_char,
    pub quic_port: u16,
    pub device_key: *const u8,
    pub device_key_len: usize,
    pub client_id: *const c_char,
    pub device_name: *const c_char,
}

/// Reads and checks a link's options into its connection's config.
///
/// # Safety
/// Every pointer in o is NULL or valid as osewa.h says.
pub unsafe fn read_link(o: &LinkOptions) -> Result<osewa_client::Config, String> {
    let addr = unsafe { opt_str(o.addr, "addr") }?.ok_or("osewa: addr is required")?;
    let server_key = unsafe { opt_str(o.server_key, "server_key") }?.ok_or("osewa: server_key is required")?;
    let client_id = unsafe { opt_str(o.client_id, "client_id") }?.ok_or("osewa: client_id is required")?;
    let name = unsafe { opt_str(o.device_name, "device_name") }?.unwrap_or_default();
    unsafe { config(&addr, &server_key, o.quic_port, o.device_key, o.device_key_len, client_id, name) }
}

/// "host:port" for the URL: the default port added when there is none, a
/// bare IPv6 address bracketed.
pub fn with_port(addr: &str) -> Result<String, String> {
    if addr.chars().any(|c| c.is_whitespace() || matches!(c, '/' | '?' | '#' | '@')) {
        return Err(format!("osewa: addr {}: want host:port", wire::quote(addr)));
    }
    if let Some(rest) = addr.strip_prefix('[') {
        return match rest.split_once(']') {
            Some((_, "")) => Ok(format!("{addr}:{DEFAULT_PORT}")),
            Some((_, p)) if p.strip_prefix(':').is_some_and(|p| p.parse::<u16>().is_ok()) => Ok(addr.to_string()),
            _ => Err(format!("osewa: addr {}: want [v6]:port", wire::quote(addr))),
        };
    }
    match addr.matches(':').count() {
        0 => Ok(format!("{addr}:{DEFAULT_PORT}")),
        1 => match addr.split_once(':') {
            Some((h, p)) if !h.is_empty() && p.parse::<u16>().is_ok() => Ok(addr.to_string()),
            _ => Err(format!("osewa: addr {}: want host:port", wire::quote(addr))),
        },
        // A bare IPv6 address.
        _ => Ok(format!("[{addr}]:{DEFAULT_PORT}")),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct AppearanceJson {
    #[serde(default)]
    color_scheme: Option<String>,
    #[serde(default)]
    fg: Option<String>,
    #[serde(default)]
    bg: Option<String>,
    #[serde(default)]
    cursor: Option<String>,
    #[serde(default)]
    palette: Vec<String>,
}

pub fn parse_appearance(j: &str) -> Result<Appearance, String> {
    let a: AppearanceJson = serde_json::from_str(j).map_err(|e| format!("osewa: appearance_json: {e}"))?;
    let dark = match a.color_scheme.as_deref() {
        None => None,
        Some("dark") => Some(true),
        Some("light") => Some(false),
        Some(s) => return Err(format!("osewa: appearance_json: color_scheme {}: want dark or light", wire::quote(s))),
    };
    let color = |s: &Option<String>| s.as_deref().map(hex_color).transpose();
    if a.palette.len() > 256 {
        return Err(format!("osewa: appearance_json: {} palette colors, at most 256", a.palette.len()));
    }
    let mut colors = TermColors { foreground: color(&a.fg)?, background: color(&a.bg)?, cursor: color(&a.cursor)?, ..Default::default() };
    for (i, c) in a.palette.iter().enumerate() {
        colors.palette[i] = Some(hex_color(c)?);
    }
    Ok(Appearance { dark, colors })
}

fn hex_color(s: &str) -> Result<[u8; 3], String> {
    let bad = || format!("osewa: appearance_json: color {}: want #rrggbb", wire::quote(s));
    let h = s.strip_prefix('#').filter(|h| h.len() == 6 && h.is_ascii()).ok_or_else(bad)?;
    let mut c = [0u8; 3];
    for (i, v) in c.iter_mut().enumerate() {
        *v = u8::from_str_radix(&h[2 * i..2 * i + 2], 16).map_err(|_| bad())?;
    }
    Ok(c)
}
