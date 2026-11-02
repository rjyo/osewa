use std::collections::HashMap;
use std::io::Write;
use std::sync::{Arc, Mutex};

use crate::release::{self, parse_version, Fetch, Release};
use crate::Error;

#[test]
fn versions_parse_and_order() {
    assert_eq!(parse_version("0.1.3"), Some([0, 1, 3]));
    assert_eq!(parse_version("v10.0.12"), Some([10, 0, 12]));
    for bad in ["", "dev", "1.2", "1.2.3.4", "1..3", "1.2.x", "+1.2.3", "1.2.3-rc1"] {
        assert_eq!(parse_version(bad), None, "{bad}");
    }
    let r = |v: &str| Release { osewa: v.into(), version: osewa_wire::VERSION, snapshot_version: crate::SNAPSHOT_VERSION };
    assert!(r("0.1.10").newer_than(Some("0.1.9")), "numeric, not string, order");
    assert!(!r("0.1.3").newer_than(Some("0.1.3")));
    assert!(!r("0.1.2").newer_than(Some("0.1.3")));
    assert!(r("0.1.0").newer_than(None), "any release beats a dev build");
    assert!(r("0.1.0").compatible());
    assert!(!Release { version: osewa_wire::VERSION + 1, ..r("0.2.0") }.compatible());
    assert!(!Release { snapshot_version: crate::SNAPSHOT_VERSION + 1, ..r("0.2.0") }.compatible());
}

/// Serves urls from a map and records what was asked.
fn fake(files: Vec<(&str, Vec<u8>)>) -> (Fetch, Arc<Mutex<Vec<String>>>) {
    let files: HashMap<String, Vec<u8>> = files.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
    let asked = Arc::new(Mutex::new(Vec::new()));
    let a = asked.clone();
    let f: Fetch = Arc::new(move |url: &str| {
        a.lock().unwrap().push(url.to_string());
        files.get(url).cloned().ok_or_else(|| Error::Other(format!("404 {url}")))
    });
    (f, asked)
}

fn gzip(b: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(b).unwrap();
    e.finish().unwrap()
}

fn sha(b: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, b).as_ref().iter().map(|x| format!("{x:02x}")).collect()
}

const DIR: &str = "https://github.com/rjyo/osewa/releases/download/v0.1.3";

#[test]
fn latest_reads_build_json() {
    let (f, asked) = fake(vec![(
        "https://github.com/rjyo/osewa/releases/latest/download/build.json",
        br#"{"ghostty":"abc","osewa":"0.1.3","snapshot_version":1,"version":0}"#.to_vec(),
    )]);
    let r = release::latest(&f).unwrap();
    assert_eq!(r, Release { osewa: "0.1.3".into(), version: 0, snapshot_version: 1 });
    assert_eq!(asked.lock().unwrap().len(), 1);

    let (f, _) = fake(vec![("https://github.com/rjyo/osewa/releases/latest/download/build.json", br#"{"osewa":"dev","version":0,"snapshot_version":1}"#.to_vec())]);
    assert!(release::latest(&f).unwrap_err().to_string().contains("not X.Y.Z"));
}

#[test]
fn binary_is_checked_and_unpacked() {
    let bin = b"\x7fELF pretend osewa".to_vec();
    let gz = gzip(&bin);
    let sums = format!("{}  osewa-linux-arm64.gz\n{}  osewa-linux-amd64.gz\n", sha(b"other"), sha(&gz));
    let (f, asked) = fake(vec![(&format!("{DIR}/SHA256SUMS"), sums.into_bytes()), (&format!("{DIR}/osewa-linux-amd64.gz"), gz.clone())]);
    assert_eq!(release::binary(&f, "0.1.3", "linux", "amd64").unwrap(), bin);
    assert_eq!(*asked.lock().unwrap(), vec![format!("{DIR}/SHA256SUMS"), format!("{DIR}/osewa-linux-amd64.gz")], "the version's own release, not latest");

    // A binary that doesn't match its line is refused.
    let (f, _) = fake(vec![
        (&format!("{DIR}/SHA256SUMS"), format!("{}  osewa-linux-amd64.gz\n", sha(b"something else")).into_bytes()),
        (&format!("{DIR}/osewa-linux-amd64.gz"), gz.clone()),
    ]);
    let e = release::binary(&f, "v0.1.3", "linux", "amd64").unwrap_err().to_string();
    assert!(e.contains("the release says"), "{e}");

    // Nor one the sums don't list.
    let (f, _) = fake(vec![(&format!("{DIR}/SHA256SUMS"), format!("{}  osewa-linux-amd64.gz\n", sha(&gz)).into_bytes())]);
    let e = release::binary(&f, "0.1.3", "darwin", "arm64").unwrap_err().to_string();
    assert!(e.contains("isn't in"), "{e}");
}
