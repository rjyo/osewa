use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::devices::{Device, Devices};
use crate::keys::{encode_key, Identity};
use crate::Error;

/// The line a device writes to `osewa enroll`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnrollRequest {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub key: String,
}

/// What `osewa enroll` prints back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnrollResponse {
    pub server_key: String,
    pub port: i64,
    pub version: i32,
}

/// Reads one EnrollRequest line from r, adds the device to d and writes the
/// EnrollResponse line to w.
pub fn enroll(mut r: impl BufRead, mut w: impl Write, id: &Identity, d: &Devices, port: i64) -> Result<(), Error> {
    let mut line = Vec::new();
    r.read_until(b'\n', &mut line).map_err(|e| Error::Msg(format!("auth: enroll: read request: {e}")))?;
    if line.is_empty() {
        return Err(Error::Msg("auth: enroll: read request: EOF".into()));
    }
    let req: EnrollRequest = osewa_wire::decode_json(&line).map_err(|e| Error::Msg(format!("auth: enroll: {e}")))?;
    d.add(Device { name: req.name, key: req.key })?;
    let resp = EnrollResponse { server_key: encode_key(&id.public()), port, version: osewa_wire::VERSION };
    let mut out = serde_json::to_vec(&resp).expect("response serializes");
    out.push(b'\n');
    w.write_all(&out)?;
    w.flush()?;
    Ok(())
}

/// `osewa enroll [--port N]`.
pub fn enroll_main(args: &[String]) -> i32 {
    let mut fs = osewa_flag::FlagSet::new("enroll");
    fs.value("port", &osewa_paths::server_port().to_string(), "port the server listens on, reported to the device");
    let port = match fs.parse(args).and_then(|p| p.int("port")) {
        Ok(port) => port,
        Err(e) => {
            if e.0 != "help requested" {
                eprintln!("{e}");
            }
            eprint!("{}", fs.usage());
            return 2;
        }
    };
    let res = Identity::load_or_create(&identity_path())
        .and_then(|id| enroll(io::stdin().lock(), io::stdout().lock(), &id, &Devices::new(devices_path()), port));
    if let Err(e) = res {
        eprintln!("osewa enroll: {e}");
        return 1;
    }
    0
}

/// Where the server keeps its key.
pub fn identity_path() -> PathBuf {
    osewa_paths::config_dir().join("server.key")
}

/// Where the server keeps the devices file.
pub fn devices_path() -> PathBuf {
    osewa_paths::config_dir().join("devices")
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::process::{Command, Stdio};
    use std::sync::Arc;

    use super::*;
    use crate::keys::decode_key;
    use crate::testutil::*;
    use crate::tls::tests::{dial, serve};
    use crate::tls::{client_config, server_config};

    #[test]
    fn enroll_device() {
        let (server, device) = (Identity::generate(), Identity::generate());
        let t = TempDir::new();
        let d = Devices::new(t.path().join("devices"));
        let req = format!("{{\"name\":\"phone\",\"key\":\"{}\"}}\n", encode_key(&device.public()));
        let mut out = Vec::new();
        enroll(Cursor::new(req), &mut out, &server, &d, 7400).unwrap();
        let resp: EnrollResponse = serde_json::from_slice(&out).unwrap();
        assert_eq!(
            resp,
            EnrollResponse { server_key: encode_key(&server.public()), port: 7400, version: osewa_wire::VERSION }
        );
        assert!(out.ends_with(b"\n"), "response is not a line");
        assert!(d.allowed(&device.public()).is_some(), "device not enrolled");

        // The enrolled pair can now talk.
        let pin = decode_key(&resp.server_key).unwrap();
        let ln = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let b = std::net::TcpStream::connect(ln.local_addr().unwrap()).unwrap();
        let (a, _) = ln.accept().unwrap();
        let srv = Arc::new(server_config(&server, &d));
        let th = std::thread::spawn(move || serve(srv, a));
        dial(Arc::new(client_config(&device, pin)), b).unwrap();
        th.join().unwrap().0.unwrap();

        // No trailing newline is fine; garbage and bad keys are not.
        enroll(Cursor::new(format!("{{\"name\":\"b\",\"key\":\"{}\"}}", rand_key())), io::sink(), &server, &d, 1)
            .unwrap();
        for input in ["", "{", r#"{"name":"c","key":"x"}"#] {
            assert!(enroll(Cursor::new(input), io::sink(), &server, &d, 1).is_err(), "enroll {input:?} succeeded");
        }
    }

    /// Runs enroll_main in a child process (stdin, stdout and the
    /// environment are process-wide) with config dir dir.
    fn run_main(dir: &std::path::Path, args: &str, stdin: &[u8]) -> (i32, Vec<u8>) {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "helper::run", "--nocapture", "--test-threads=1"])
            .env("OSEWA_AUTH_HELPER", "enroll_main")
            .env("OSEWA_ARGS", args)
            .env("OSEWA_CONFIG_DIR", dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        std::io::Write::write_all(child.stdin.as_mut().unwrap(), stdin).unwrap();
        let out = child.wait_with_output().unwrap();
        // The response follows the test harness's "test helper::run ... ".
        let line = match out.stdout.iter().position(|&c| c == b'{') {
            Some(i) => out.stdout[i..].split(|&c| c == b'\n').next().unwrap_or_default().to_vec(),
            None => Vec::new(),
        };
        (out.status.code().unwrap(), line)
    }

    #[test]
    fn enroll_main_cli() {
        let t = TempDir::new();
        let device = Identity::generate();
        let req = format!("{{\"name\":\"phone\",\"key\":\"{}\"}}\n", encode_key(&device.public()));
        let (code, out) = run_main(t.path(), "--port 9000", req.as_bytes());
        assert_eq!(code, 0);
        let resp: EnrollResponse = serde_json::from_slice(&out).unwrap();
        let id = Identity::load_or_create(&t.path().join("server.key")).unwrap();
        assert!(resp.port == 9000 && resp.server_key == encode_key(&id.public()), "response {resp:?}");
        assert!(Devices::new(t.path().join("devices")).allowed(&device.public()).is_some(), "not enrolled");

        assert_eq!(run_main(t.path(), "--bogus", b"").0, 2, "bad flag");
        assert_eq!(run_main(t.path(), "", b"").0, 1, "empty stdin succeeded");
    }
}
