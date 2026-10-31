use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::keys::{decode_key, PublicKey};
use crate::{create_temp, dir_of, io_at, Error};

/// One enrolled device: a line of JSON in the devices file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub key: String,
}

/// The devices file. It holds no state beyond its path, so every call sees
/// the file as it is now.
#[derive(Debug, Clone)]
pub struct Devices {
    pub path: PathBuf,
}

/// Bounds a device name.
pub(crate) const MAX_NAME: usize = 64;

impl Devices {
    pub fn new(path: impl Into<PathBuf>) -> Devices {
        Devices { path: path.into() }
    }

    /// Every enrolled device. A missing file is an empty list.
    pub fn list(&self) -> Result<Vec<Device>, Error> {
        strict(dir_of(&self.path))?;
        let b = match fs::read(&self.path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(io_at("open", &self.path, e)),
            Ok(b) => b,
        };
        // Whoever can write the devices file can enroll themselves, so it and
        // its directory must be ours alone, as sshd demands of authorized_keys.
        strict(&self.path)?;
        let mut out = Vec::new();
        for (i, line) in b.split(|&c| c == b'\n').enumerate() {
            let line = line.trim_ascii();
            if line.is_empty() {
                continue;
            }
            // Leniently, as v0 peers decode JSON: any key case, last duplicate
            // wins, invalid UTF-8 as U+FFFD.
            let dev = osewa_wire::decode_json(line)
                .map_err(|e| Error::Msg(format!("auth: {}:{}: {e}", self.path.display(), i + 1)))?;
            out.push(dev);
        }
        Ok(out)
    }

    /// Enrolls a device, replacing any entry with the same name or the same
    /// key. One entry per key means revoking by name revokes the key: a
    /// device that re-enrolled under a new hostname leaves no older entry
    /// behind.
    pub fn add(&self, dev: Device) -> Result<(), Error> {
        valid_name(&dev.name)?;
        let k = decode_key(&dev.key)?;
        self.update(|mut list| {
            list.retain(|e| e.name != dev.name && decode_key(&e.key).map_or(true, |ek| ek != k));
            list.push(dev);
            Ok(list)
        })
    }

    /// Revokes the device with this name.
    pub fn remove(&self, name: &str) -> Result<(), Error> {
        self.update(|mut list| {
            let n = list.len();
            list.retain(|e| e.name != name);
            if list.len() == n {
                return Err(Error::Msg(format!("auth: no device {name:?}")));
            }
            Ok(list)
        })
    }

    /// The enrolled device with key k, if any. It reads the file on every
    /// call so a revocation applies to the next connection.
    pub fn allowed(&self, k: &PublicKey) -> Option<Device> {
        self.find(k).ok()
    }

    /// allowed with the reason: Error::UnknownDevice, or why the devices
    /// file can't be trusted.
    pub fn find(&self, k: &PublicKey) -> Result<Device, Error> {
        self.list()?.into_iter().find(|e| decode_key(&e.key).is_ok_and(|ek| &ek == k)).ok_or(Error::UnknownDevice)
    }

    /// Rewrites the devices file with change(current list), holding the lock
    /// throughout so a concurrent add or remove in another process can't be
    /// lost.
    fn update(&self, change: impl FnOnce(Vec<Device>) -> Result<Vec<Device>, Error>) -> Result<(), Error> {
        let _lock = self.lock()?;
        let list = change(self.list()?)?;
        self.write(&list)
    }

    /// Takes an exclusive flock on a sibling lock file, creating the
    /// directory if needed. flock also excludes other descriptors in the
    /// same process. Dropping the file releases the lock.
    fn lock(&self) -> Result<File, Error> {
        let dir = dir_of(&self.path);
        fs::DirBuilder::new().recursive(true).mode(0o700).create(dir).map_err(|e| io_at("mkdir", dir, e))?;
        let mut p = self.path.clone().into_os_string();
        p.push(".lock");
        let p = PathBuf::from(p);
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&p)
            .map_err(|e| io_at("open", &p, e))?;
        loop {
            if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } == 0 {
                return Ok(f);
            }
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(e.into());
            }
        }
    }

    /// Replaces the devices file whole, under the lock (which made the
    /// directory).
    fn write(&self, list: &[Device]) -> Result<(), Error> {
        let mut buf = String::new();
        for e in list {
            buf.push_str(&device_json(e));
            buf.push('\n');
        }
        let (mut f, tmp) = create_temp(dir_of(&self.path), ".devices-")?;
        f.write_all(buf.as_bytes()).map_err(|e| io_at("write", &tmp.0, e))?;
        drop(f);
        // create_temp already made the file 0600.
        fs::rename(&tmp.0, &self.path).map_err(|e| io_at("rename", &self.path, e))
    }
}

/// A device as one devices-file line: compact JSON with <, >, & and
/// U+2028/U+2029 escaped, as the v0 file has them, so every implementation
/// sharing the file writes the same bytes.
fn device_json(d: &Device) -> String {
    let s = serde_json::to_string(d).expect("strings serialize");
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '<' => out.push_str("\\u003c"),
            '>' => out.push_str("\\u003e"),
            '&' => out.push_str("\\u0026"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c => out.push(c),
        }
    }
    out
}

/// Keeps device names to short printable text: they end up in files and on
/// screens.
pub(crate) fn valid_name(name: &str) -> Result<(), Error> {
    if name.is_empty() || name.len() > MAX_NAME {
        return Err(Error::Msg(format!("auth: device name must be 1-{MAX_NAME} bytes of UTF-8")));
    }
    if name.chars().any(char::is_control) {
        return Err(Error::Msg(format!("auth: device name {name:?} has control characters")));
    }
    Ok(())
}

/// Refuses a path (following symlinks) that belongs to another user or that
/// group or others can write. A missing path is fine.
fn strict(path: &Path) -> Result<(), Error> {
    let md = match fs::metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(io_at("stat", path, e)),
        Ok(md) => md,
    };
    if md.uid() != unsafe { libc::getuid() } {
        return Err(Error::Msg(format!("auth: {} is not owned by this user", path.display())));
    }
    let perm = md.permissions().mode() & 0o777;
    if perm & 0o022 != 0 {
        return Err(Error::Msg(format!(
            "auth: {} is writable by others (mode {perm:04o}); chmod go-w it",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::{encode_key, Identity};
    use crate::testutil::*;

    #[test]
    fn devices() {
        let t = TempDir::new();
        let d = Devices::new(t.path().join("cfg/devices"));
        assert!(d.list().unwrap().is_empty());
        let (k1, k2, k3) = (rand_key(), rand_key(), rand_key());
        d.add(Device { name: "phone".into(), key: k1.clone() }).unwrap();
        d.add(Device { name: "laptop".into(), key: k2 }).unwrap();
        d.add(Device { name: "phone".into(), key: k3.clone() }).unwrap(); // replaces
        let l = d.list().unwrap();
        assert!(l.len() == 2 && l[0].name == "laptop" && l[1].key == k3, "list {l:?}");
        assert_eq!(mode(&d.path), 0o600);
        let pk1 = decode_key(&k1).unwrap();
        let pk3 = decode_key(&k3).unwrap();
        assert!(d.allowed(&pk1).is_none(), "replaced key still allowed");
        assert_eq!(d.allowed(&pk3).expect("new key not allowed").name, "phone");
        d.remove("phone").unwrap();
        assert!(d.allowed(&pk3).is_none(), "removed key allowed");
        assert!(d.remove("phone").is_err(), "removing missing device succeeded");
        assert!(d.add(Device { name: "".into(), key: k1 }).is_err(), "nameless device added");
        assert!(d.add(Device { name: "x".into(), key: "bad".into() }).is_err(), "bad key added");
    }

    #[test]
    fn devices_corrupt() {
        let t = TempDir::new();
        let d = Devices::new(t.path().join("devices"));
        write_mode(&d.path, b"\n{not json\n", 0o600);
        let err = d.list().expect_err("corrupt file listed");
        assert!(err.to_string().contains(":2:"), "{err}");
        assert!(d.add(Device { name: "a".into(), key: rand_key() }).is_err(), "add over corrupt file");
        assert!(d.remove("a").is_err(), "remove over corrupt file");
        assert!(d.allowed(&Identity::generate().public()).is_none(), "allowed from corrupt file");
    }

    #[test]
    fn remove_revokes_key_under_every_name() {
        let t = TempDir::new();
        let d = Devices::new(t.path().join("devices"));
        let key = rand_key();
        d.add(Device { name: "old-hostname".into(), key: key.clone() }).unwrap();
        d.add(Device { name: "new-hostname".into(), key: key.clone() }).unwrap();
        let l = d.list().unwrap();
        assert!(l.len() == 1 && l[0].name == "new-hostname", "one entry per key, got {l:?}");
        d.remove("new-hostname").unwrap();
        assert!(d.allowed(&decode_key(&key).unwrap()).is_none(), "revoked key still allowed");
    }

    #[test]
    fn device_names() {
        let t = TempDir::new();
        let d = Devices::new(t.path().join("devices"));
        let k = rand_key();
        let long = "n".repeat(MAX_NAME + 1);
        for name in ["", "evil\x1b]52;c;aGk=\x07", "a\nb", "c\u{9b}d", long.as_str()] {
            assert!(d.add(Device { name: name.into(), key: k.clone() }).is_err(), "name {name:?} accepted");
        }
        // A name with invalid UTF-8 can't be built: a Rust String is always
        // UTF-8, and the JSON decoder refuses invalid UTF-8 before it gets
        // here.
        d.add(Device { name: "jyo's iPhone 15".into(), key: k }).unwrap();
    }

    // A devices file others can write would let them enroll themselves.
    #[test]
    fn devices_strict_modes() {
        let t = TempDir::new();
        let dir = t.path().join("osewa");
        let d = Devices::new(dir.join("devices"));
        let id = Identity::generate();
        d.add(Device { name: "phone".into(), key: encode_key(&id.public()) }).unwrap();
        assert!(d.allowed(&id.public()).is_some(), "enrolled device refused");
        for (path, m) in [(&d.path, 0o622), (&d.path, 0o606), (&dir, 0o770), (&dir, 0o707)] {
            let orig = mode(path);
            chmod(path, m);
            assert!(d.allowed(&id.public()).is_none(), "{} at {m:04o} still trusted", path.display());
            let err = d.find(&id.public()).expect_err("trusted");
            assert!(err.to_string().contains("writable by others"), "{} at {m:04o}: {err}", path.display());
            assert!(
                d.add(Device { name: "intruder".into(), key: rand_key() }).is_err(),
                "enrolled into {} at {m:04o}",
                path.display()
            );
            chmod(path, orig);
        }
        // Readable by others is fine: keys in it are public.
        chmod(&d.path, 0o644);
        assert!(d.allowed(&id.public()).is_some(), "0644 devices file refused");
    }

    #[test]
    fn writes_v0_bytes() {
        let d = Device { name: "a<b>&c\u{2028}".into(), key: "k".into() };
        assert_eq!(device_json(&d), "{\"name\":\"a\\u003cb\\u003e\\u0026c\\u2028\",\"key\":\"k\"}");
    }
}
