//! osewa's GitHub releases (scripts/release.sh publishes them): which is
//! the latest, and one platform's build of a release, checked against the
//! release's SHA256SUMS. Downloads go through curl or wget, as install.sh's
//! do, so the library carries no HTTP stack of its own.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::Arc;

use ring::digest;

use crate::Error;

/// Where the releases are; $OSEWA_RELEASES overrides it (a mirror, a test).
pub const RELEASES: &str = "https://github.com/rjyo/osewa/releases";

/// No osewa build is anywhere near this; a gzip that says otherwise is
/// not one.
const MAX_BINARY: u64 = 256 << 20;

/// Gets a URL's body.
pub type Fetch = Arc<dyn Fn(&str) -> Result<Vec<u8>, Error> + Send + Sync>;

/// This build's release version ("0.1.3"), or None for a build that isn't
/// one (OSEWA_VERSION unset when it was built).
pub fn current() -> Option<&'static str> {
    option_env!("OSEWA_VERSION").filter(|v| parse_version(v).is_some())
}

fn base() -> String {
    std::env::var("OSEWA_RELEASES").ok().filter(|b| !b.is_empty()).unwrap_or_else(|| RELEASES.into())
}

/// Fetches with curl, else wget.
pub fn fetch() -> Fetch {
    Arc::new(fetch_url)
}

fn fetch_url(url: &str) -> Result<Vec<u8>, Error> {
    let tries: [(&str, &[&str]); 2] = [
        ("curl", &["-fsSL", "--retry", "2", "--connect-timeout", "15", "--max-time", "300"]),
        ("wget", &["-q", "-T", "60", "-O", "-"]),
    ];
    for (prog, args) in tries {
        let out = match Command::new(prog).args(args).arg(url).stdin(Stdio::null()).output() {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(Error::Other(format!("{prog} {url}: {e}"))),
            Ok(o) => o,
        };
        if !out.status.success() {
            let why = String::from_utf8_lossy(&out.stderr).trim().to_string();
            return Err(Error::Other(format!("{prog} {url}: {}", if why.is_empty() { out.status.to_string() } else { why })));
        }
        return Ok(out.stdout);
    }
    Err(Error::Other(format!("downloading {url} needs curl or wget")))
}

/// "1.2.3" or "v1.2.3" as numbers.
pub fn parse_version(v: &str) -> Option<[u64; 3]> {
    let v = v.strip_prefix('v').unwrap_or(v);
    let mut it = v.split('.').map(|p| if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) { None } else { p.parse().ok() });
    let r = [it.next()??, it.next()??, it.next()??];
    it.next().is_none().then_some(r)
}

/// A release, as its build.json (`osewa version --json`) describes it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Release {
    /// "0.1.3"
    pub osewa: String,
    /// The protocol version.
    pub version: i32,
    pub snapshot_version: i32,
}

impl Release {
    /// Whether this build and the release interoperate, so a server can
    /// move to it without stranding sessions or clients.
    pub fn compatible(&self) -> bool {
        self.version == osewa_wire::VERSION && self.snapshot_version == crate::SNAPSHOT_VERSION
    }

    /// Whether it is a later release than `current`; any release is later
    /// than a build that isn't one.
    pub fn newer_than(&self, current: Option<&str>) -> bool {
        match (parse_version(&self.osewa), current.and_then(parse_version)) {
            (Some(r), Some(c)) => r > c,
            (Some(_), None) => true,
            _ => false,
        }
    }
}

/// The latest release.
pub fn latest(fetch: &Fetch) -> Result<Release, Error> {
    let url = format!("{}/latest/download/build.json", base());
    let body = fetch(&url)?;
    let r: Release = serde_json::from_slice(&body).map_err(|e| Error::Other(format!("{url}: {e}")))?;
    if parse_version(&r.osewa).is_none() {
        return Err(Error::Other(format!("{url}: version {} is not X.Y.Z", osewa_wire::quote(&r.osewa))));
    }
    Ok(r)
}

/// Release `version`'s osewa for os/arch ("linux", "amd64"), checked
/// against the release's SHA256SUMS and unpacked.
pub fn binary(fetch: &Fetch, version: &str, os: &str, arch: &str) -> Result<Vec<u8>, Error> {
    let tag = format!("v{}", version.strip_prefix('v').unwrap_or(version));
    let dir = format!("{}/download/{tag}", base());
    let asset = format!("osewa-{os}-{arch}.gz");
    let sums = fetch(&format!("{dir}/SHA256SUMS"))?;
    let want = String::from_utf8_lossy(&sums)
        .lines()
        .find_map(|l| {
            let (sum, name) = l.split_once(char::is_whitespace)?;
            (name.trim_start().trim_start_matches('*') == asset).then(|| sum.to_ascii_lowercase())
        })
        .ok_or_else(|| Error::Other(format!("{asset} isn't in osewa {tag}'s SHA256SUMS")))?;
    let gz = fetch(&format!("{dir}/{asset}"))?;
    let got = hex(digest::digest(&digest::SHA256, &gz).as_ref());
    if got != want {
        return Err(Error::Other(format!("{asset} of osewa {tag} has SHA-256 {got}, the release says {want}")));
    }
    let mut bin = Vec::new();
    flate2::read::GzDecoder::new(&gz[..])
        .take(MAX_BINARY + 1)
        .read_to_end(&mut bin)
        .map_err(|e| Error::Other(format!("unpacking {asset} of osewa {tag}: {e}")))?;
    if bin.len() as u64 > MAX_BINARY {
        return Err(Error::Other(format!("{asset} of osewa {tag} unpacks to more than {MAX_BINARY} bytes")));
    }
    Ok(bin)
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
