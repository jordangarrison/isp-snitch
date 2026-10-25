//! HTTP API tests against a seeded in-memory store.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use isp_snitch::store::{HopMinute, Store};
use isp_snitch::web::{AppState, router};
use serde_json::Value;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tower::ServiceExt;

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn row(target_id: i64, ts: i64, ttl: u8, sent: u32, recv: u32, reached: bool) -> HopMinute {
    HopMinute {
        target_id,
        ts,
        ttl,
        sent,
        recv,
        rtt_sum_us: recv as u64 * 10_000,
        rtt_sumsq_us2: 0.0,
        rtt_min_us: (recv > 0).then_some(5_000),
        rtt_max_us: (recv > 0).then_some(20_000),
        addr: (recv > 0).then(|| format!("10.0.0.{ttl}")),
        addr_count: (recv > 0) as u32,
        reached,
        send_errors: 0,
    }
}

/// One target, 3 hops, for the 60 minutes before the current minute. Hop 2
/// rate-limits (30% raw loss that does not carry through). In the minute at
/// `lossy` the target loses 50%.
struct Seed {
    state: AppState,
    id: i64,
    start: i64,
    end: i64,
    lossy: i64,
}

fn seed() -> Seed {
    let store = Arc::new(Store::open_in_memory().unwrap());
    let id = store
        .ensure_target("Cloudflare", "1.1.1.1".parse().unwrap())
        .unwrap();
    let end = now() / 60 * 60;
    let start = end - 3600;
    let lossy = start + 30 * 60;
    let mut rows = Vec::new();
    for m in 0..60 {
        let ts = start + m * 60;
        let target_recv = if ts == lossy { 30 } else { 60 };
        rows.push(row(id, ts, 1, 60, 60, false));
        rows.push(row(id, ts, 2, 60, 42, false));
        rows.push(row(id, ts, 3, 60, target_recv, true));
    }
    store.insert_minutes(&rows).unwrap();
    let targets = store.targets().unwrap();
    Seed {
        state: AppState::new(store, targets, 90),
        id,
        start,
        end,
        lossy,
    }
}

async fn get(state: &AppState, uri: &str) -> (StatusCode, Value) {
    let resp = router(state.clone())
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let v = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, v)
}

#[tokio::test]
async fn healthz_ok() {
    let s = seed();
    let resp = router(s.state.clone())
        .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn targets_summary_reports_loss() {
    let s = seed();
    let (st, v) = get(
        &s.state,
        &format!("/api/targets?from={}&to={}", s.start, s.end),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let t = &v["targets"][0];
    assert_eq!(t["name"], "Cloudflare");
    assert_eq!(t["addr"], "1.1.1.1");
    // 30 lost of 3600 sent at the target.
    let loss = t["loss_pct"].as_f64().unwrap();
    assert!((loss - 100.0 * 30.0 / 3600.0).abs() < 1e-6, "{loss}");
    assert_eq!(t["lossy_buckets"], 1);
    assert_eq!(t["avg_ms"].as_f64().unwrap(), 10.0);
}

#[tokio::test]
async fn series_has_nulls_for_gaps() {
    let s = seed();
    // Range starts 10 minutes before the seeded data: those buckets are gaps.
    let from = s.start - 600;
    let (st, v) = get(
        &s.state,
        &format!("/api/targets/{}/series?from={from}&to={}", s.id, s.end),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["step"], 60);
    let ts = v["ts"].as_array().unwrap();
    let loss = v["loss_pct"].as_array().unwrap();
    assert_eq!(ts.len(), 70);
    assert_eq!(loss.len(), 70);
    assert!(loss[0].is_null());
    assert_eq!(loss[10].as_f64().unwrap(), 0.0);
    let i = ts.iter().position(|t| t.as_i64() == Some(s.lossy)).unwrap();
    assert_eq!(loss[i].as_f64().unwrap(), 50.0);
    assert!(v["avg_ms"][0].is_null());
}

#[tokio::test]
async fn hops_flags_rate_limited() {
    let s = seed();
    let (st, v) = get(
        &s.state,
        &format!("/api/targets/{}/hops?from={}&to={}", s.id, s.start, s.end),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let hops = v["hops"].as_array().unwrap();
    assert_eq!(hops.len(), 3);
    let h2 = &hops[1];
    assert_eq!(h2["ttl"], 2);
    assert_eq!(h2["addr"], "10.0.0.2");
    assert_eq!(h2["rate_limited"][0], true);
    assert_eq!(h2["eff_loss_pct"][0].as_f64().unwrap(), 0.0);
    assert!((h2["loss_pct"][0].as_f64().unwrap() - 30.0).abs() < 1e-9);
    assert_eq!(hops[2]["ttl"], 3);
}

#[tokio::test]
async fn slice_returns_mtr_table() {
    let s = seed();
    let (st, v) = get(
        &s.state,
        &format!(
            "/api/targets/{}/slice?from={}&to={}",
            s.id,
            s.lossy,
            s.lossy + 60
        ),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let hops = v["hops"].as_array().unwrap();
    assert_eq!(hops.len(), 3);
    assert_eq!(hops[2]["loss_pct"].as_f64().unwrap(), 50.0);
    assert_eq!(hops[2]["eff_loss_pct"].as_f64().unwrap(), 50.0);
    // Target loses 50% here, so hop 2's 30% is not below later hops: not rate-limited.
    assert_eq!(hops[1]["rate_limited"], false);
    assert_eq!(hops[1]["eff_loss_pct"].as_f64().unwrap(), 30.0);
    assert_eq!(hops[0]["addr_changed"], false);
}

#[tokio::test]
async fn empty_db_returns_empty_arrays() {
    let store = Arc::new(Store::open_in_memory().unwrap());
    let id = store
        .ensure_target("X", "9.9.9.9".parse().unwrap())
        .unwrap();
    let state = AppState::new(store.clone(), store.targets().unwrap(), 90);
    let (st, v) = get(&state, "/api/targets").await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(v["targets"][0]["loss_pct"].is_null());
    let (st, v) = get(&state, &format!("/api/targets/{id}/hops")).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["hops"].as_array().unwrap().len(), 0);
    let (st, v) = get(&state, &format!("/api/targets/{id}/series")).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(v["loss_pct"].as_array().unwrap().iter().all(Value::is_null));
    let (st, v) = get(&state, &format!("/api/targets/{id}/slice")).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["hops"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn bad_params_are_400() {
    let s = seed();
    for q in [
        "from=100&to=100".to_string(),
        "from=200&to=100".to_string(),
        "from=abc".to_string(),
        format!("from={}&to={}", s.end - 401 * 86_400, s.end),
    ] {
        let (st, v) = get(&s.state, &format!("/api/targets/{}/series?{q}", s.id)).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{q}: {v}");
        assert!(v["error"].is_string(), "{q}: {v}");
    }
}

#[tokio::test]
async fn unknown_target_is_404() {
    let s = seed();
    let (st, v) = get(&s.state, "/api/targets/999/hops").await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert!(v["error"].is_string());
    let (st, _) = get(&s.state, "/api/targets/abc/hops").await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn ui_assets_served() {
    let s = seed();
    for (uri, ctype) in [
        ("/", "text/html"),
        ("/target/1", "text/html"),
        ("/assets/app.css", "text/css"),
        ("/assets/overview.js", "text/javascript"),
        ("/assets/target.js", "text/javascript"),
        ("/assets/vendor/uplot/uPlot.iife.min.js", "text/javascript"),
    ] {
        let resp = router(s.state.clone())
            .oneshot(Request::get(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{uri}");
        let ct = resp.headers()["content-type"].to_str().unwrap().to_string();
        assert!(ct.starts_with(ctype), "{uri}: {ct}");
    }
    let resp = router(s.state.clone())
        .oneshot(Request::get("/assets/nope.js").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// Target normally at TTL 3; for one minute the path is one hop shorter.
/// Hop 2 rate-limits (30% raw loss) the whole time. The range summary must
/// not report hop 2's loss as target loss.
#[tokio::test]
async fn path_change_does_not_blame_rate_limited_router() {
    let store = Arc::new(Store::open_in_memory().unwrap());
    let id = store
        .ensure_target("CF", "1.1.1.1".parse().unwrap())
        .unwrap();
    let end = now() / 60 * 60;
    let start = end - 3600;
    let mut rows = Vec::new();
    for m in 0..60 {
        let ts = start + m * 60;
        rows.push(row(id, ts, 1, 60, 60, false));
        if m == 30 {
            rows.push(row(id, ts, 2, 60, 60, true));
        } else {
            rows.push(row(id, ts, 2, 60, 42, false));
            rows.push(row(id, ts, 3, 60, 60, true));
        }
    }
    store.insert_minutes(&rows).unwrap();
    let state = AppState::new(store.clone(), store.targets().unwrap(), 90);
    let (st, v) = get(&state, &format!("/api/targets?from={start}&to={end}")).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let loss = v["targets"][0]["loss_pct"].as_f64().unwrap();
    assert!(loss < 0.01, "summary blamed the router: {loss}");
    assert_eq!(v["targets"][0]["lossy_buckets"], 0);
}
