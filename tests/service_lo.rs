//! End-to-end: the full service probing 127.0.0.1 over `lo` writes rows to
//! SQLite and flushes the current minute on shutdown.

use isp_snitch::config::Config;
use isp_snitch::icmp::socket::PingSocket;
use isp_snitch::service;
use isp_snitch::store::{Store, Table};
use std::net::Ipv4Addr;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn service_records_loopback_rounds() {
    if let Err(e) = PingSocket::open(Ipv4Addr::LOCALHOST, Some("lo")) {
        eprintln!("skipped: ping sockets not available ({e})");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("svc.db");
    let cfg = Config::from_toml(&format!(
        r#"
        interface = "lo"
        listen = "127.0.0.1:0"
        db_path = "{}"
        [probe]
        interval_ms = 200
        timeout_ms = 500
        max_ttl = 4
        [[targets]]
        name = "loopback"
        addr = "127.0.0.1"
        "#,
        db.display()
    ))
    .unwrap();

    service::run_until(cfg, tokio::time::sleep(Duration::from_millis(2500)))
        .await
        .unwrap();

    let store = Store::open(&db).unwrap();
    let t = store.targets().unwrap();
    assert_eq!(t.len(), 1);
    let rows = store
        .hop_buckets(Table::Minute, t[0].id, 0, i64::MAX / 2, 60)
        .unwrap();
    let sent: u32 = rows.iter().map(|r| r.sent).sum();
    let recv: u32 = rows.iter().map(|r| r.recv).sum();
    assert!(sent >= 5, "sent {sent}");
    assert_eq!(sent, recv, "loopback never loses");
    assert!(rows.iter().all(|r| r.ttl == 1 && r.reached));
}
