//! A host built from another libghostty commit.

use osewa_carrier::ws;
use osewa_client::hosts;
use osewa_wire as wire;

use super::*;

/// A host built from another libghostty commit gets a clear hint.
#[test]
fn run_incompatible_host() {
    if std::env::var("OSEWA_CARRIER").is_ok_and(|c| c == "quic") {
        eprintln!("skipping: the incompatible host is a WebSocket-only fake");
        return;
    }
    let (host, client) = (TempDir::new(), TempDir::new());
    let sid = osewa_auth::Identity::load_or_create(&host.path().join("server.key")).unwrap();
    let cid = osewa_auth::Identity::load_or_create(&hosts::key_path(client.path())).unwrap();
    let devices = osewa_auth::Devices::new(host.path().join("devices"));
    devices.add(osewa_auth::Device { name: "test".into(), key: osewa_auth::encode_key(&cid.public()) }).unwrap();
    let tls = Arc::new(osewa_auth::server_config(&sid, &devices));
    let ln = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    ln.set_nonblocking(true).unwrap();
    let addr = ln.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        tokio::task::LocalSet::new().block_on(&rt, async move {
            let ln = tokio::net::TcpListener::from_std(ln).unwrap();
            let acc = tokio_rustls::TlsAcceptor::from(tls);
            while let Ok((tcp, _)) = ln.accept().await {
                let acc = acc.clone();
                tokio::task::spawn_local(async move {
                    let Ok(s) = acc.accept(tcp).await else { return };
                    let Ok(c) = ws::accept(s, false).await else { return };
                    let _ = c.read().await; // HELLO
                    let w = wire::Welcome {
                        version: wire::VERSION,
                        snapshot_version: osewa_ghostty::SNAPSHOT_VERSION,
                        ghostty: "0123abcd".into(),
                        ..Default::default()
                    };
                    let _ = c.write(&wire::json(wire::TYPE_WELCOME, 0, &w).marshal()).await;
                    let _ = c.read().await;
                });
            }
        });
    });
    hosts::save(
        client.path(),
        hosts::Host { name: "old".into(), addr, server_key: osewa_auth::encode_key(&sid.public()), ..Default::default() },
    )
    .unwrap();
    let (code, _, err) = run(client.path(), &["ls", "old"]);
    assert_eq!(code, 1, "{err}");
    for want in ["0123abcd", osewa_ghostty::COMMIT, "update osewa on the host or here"] {
        assert!(err.contains(want), "missing {want:?} in {err:?}");
    }
}
