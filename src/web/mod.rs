//! HTTP API and UI routes. JSON shapes are documented in docs/api.md.

use crate::analysis::{
    HopBucket, HopStat, MAX_POINTS, Table, analyze, merge_by_ttl, pick_step, pick_table,
    target_point,
};
use crate::store::{Store, TargetRow};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_RANGE_SECS: i64 = 400 * 86_400;
const DEFAULT_RANGE_SECS: i64 = 86_400;

#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

struct Inner {
    store: Arc<Store>,
    targets: Vec<TargetRow>,
    minute_days: u32,
}

impl AppState {
    pub fn new(store: Arc<Store>, targets: Vec<TargetRow>, minute_days: u32) -> Self {
        Self {
            inner: Arc::new(Inner {
                store,
                targets,
                minute_days,
            }),
        }
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/api/targets", get(targets))
        .route("/api/targets/{id}/series", get(series))
        .route("/api/targets/{id}/hops", get(hops))
        .route("/api/targets/{id}/slice", get(slice))
        .with_state(state)
}

pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    fn internal(e: impl std::fmt::Display) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("internal error: {e}"),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

type ApiResult<T> = Result<Json<T>, ApiError>;
type Params = Query<HashMap<String, String>>;

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn parse_param(q: &HashMap<String, String>, key: &str) -> Result<Option<i64>, ApiError> {
    q.get(key)
        .map(|v| {
            v.parse::<i64>().map_err(|_| {
                ApiError::bad_request(format!("{key} must be unix seconds, got {v:?}"))
            })
        })
        .transpose()
}

fn range(q: &HashMap<String, String>, now: i64) -> Result<(i64, i64), ApiError> {
    let to = parse_param(q, "to")?.unwrap_or(now);
    let from = parse_param(q, "from")?.unwrap_or(to - DEFAULT_RANGE_SECS);
    if from >= to {
        return Err(ApiError::bad_request("from must be before to"));
    }
    if to - from > MAX_RANGE_SECS {
        return Err(ApiError::bad_request("range must be 400 days or less"));
    }
    Ok((from, to))
}

fn find_target(state: &AppState, id: &str) -> Result<TargetRow, ApiError> {
    let id: i64 = id
        .parse()
        .map_err(|_| ApiError::bad_request(format!("target id must be a number, got {id:?}")))?;
    state
        .inner
        .targets
        .iter()
        .find(|t| t.id == id)
        .cloned()
        .ok_or_else(|| ApiError {
            status: StatusCode::NOT_FOUND,
            message: format!("no target with id {id}"),
        })
}

/// Rows for one target over a range, split into display buckets.
struct Grid {
    step: i64,
    ts: Vec<i64>,
    buckets: Vec<Vec<HopBucket>>,
}

fn grid(state: &AppState, target_id: i64, from: i64, to: i64) -> anyhow::Result<Grid> {
    let step = pick_step(from, to, MAX_POINTS);
    let table = pick_table(from, now(), step, state.inner.minute_days);
    let start = from.div_euclid(step) * step;
    let n = ((to - start) + step - 1) / step;
    let ts: Vec<i64> = (0..n).map(|i| start + i * step).collect();
    let mut buckets = vec![Vec::new(); n as usize];
    for row in state
        .inner
        .store
        .hop_buckets(table, target_id, start, to, step)?
    {
        let i = ((row.ts - start) / step) as usize;
        if let Some(b) = buckets.get_mut(i) {
            b.push(row);
        }
    }
    Ok(Grid { step, ts, buckets })
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(ApiError::internal)?
        .map_err(ApiError::internal)
}

#[derive(Serialize)]
struct TargetSummary {
    id: i64,
    name: String,
    addr: String,
    loss_pct: Option<f64>,
    avg_ms: Option<f64>,
    max_ms: Option<f64>,
    lossy_buckets: usize,
}

#[derive(Serialize)]
struct TargetsResponse {
    from: i64,
    to: i64,
    step: i64,
    targets: Vec<TargetSummary>,
}

async fn targets(State(state): State<AppState>, Query(q): Params) -> ApiResult<TargetsResponse> {
    let (from, to) = range(&q, now())?;
    let st = state.clone();
    let out = blocking(move || {
        let mut targets = Vec::new();
        for t in &st.inner.targets {
            let g = grid(&st, t.id, from, to)?;
            let lossy_buckets = g
                .buckets
                .iter()
                .filter_map(|b| target_point(&analyze(b)))
                .filter(|p| p.loss_pct > 0.0)
                .count();
            let all: Vec<HopBucket> = g.buckets.into_iter().flatten().collect();
            let point = target_point(&analyze(&merge_by_ttl(&all)));
            targets.push(TargetSummary {
                id: t.id,
                name: t.name.clone(),
                addr: t.addr.to_string(),
                loss_pct: point.as_ref().map(|p| p.loss_pct),
                avg_ms: point.as_ref().and_then(|p| p.avg_ms),
                max_ms: point.as_ref().and_then(|p| p.max_ms),
                lossy_buckets,
            });
        }
        Ok(TargetsResponse {
            from,
            to,
            step: pick_step(from, to, MAX_POINTS),
            targets,
        })
    })
    .await?;
    Ok(Json(out))
}

#[derive(Serialize)]
struct SeriesResponse {
    step: i64,
    ts: Vec<i64>,
    loss_pct: Vec<Option<f64>>,
    avg_ms: Vec<Option<f64>>,
    max_ms: Vec<Option<f64>>,
}

async fn series(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Params,
) -> ApiResult<SeriesResponse> {
    let target = find_target(&state, &id)?;
    let (from, to) = range(&q, now())?;
    let out = blocking(move || {
        let g = grid(&state, target.id, from, to)?;
        let points: Vec<_> = g
            .buckets
            .iter()
            .map(|b| target_point(&analyze(b)))
            .collect();
        Ok(SeriesResponse {
            step: g.step,
            ts: g.ts,
            loss_pct: points
                .iter()
                .map(|p| p.as_ref().map(|p| p.loss_pct))
                .collect(),
            avg_ms: points
                .iter()
                .map(|p| p.as_ref().and_then(|p| p.avg_ms))
                .collect(),
            max_ms: points
                .iter()
                .map(|p| p.as_ref().and_then(|p| p.max_ms))
                .collect(),
        })
    })
    .await?;
    Ok(Json(out))
}

#[derive(Serialize)]
struct HopSeries {
    ttl: u8,
    addr: Option<String>,
    loss_pct: Vec<Option<f64>>,
    eff_loss_pct: Vec<Option<f64>>,
    rate_limited: Vec<bool>,
    avg_ms: Vec<Option<f64>>,
}

#[derive(Serialize)]
struct HopsResponse {
    step: i64,
    ts: Vec<i64>,
    hops: Vec<HopSeries>,
}

async fn hops(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Params,
) -> ApiResult<HopsResponse> {
    let target = find_target(&state, &id)?;
    let (from, to) = range(&q, now())?;
    let out = blocking(move || {
        let g = grid(&state, target.id, from, to)?;
        let stats: Vec<BTreeMap<u8, HopStat>> = g
            .buckets
            .iter()
            .map(|b| analyze(b).into_iter().map(|s| (s.ttl, s)).collect())
            .collect();
        let ttls: BTreeSet<u8> = stats.iter().flat_map(|m| m.keys().copied()).collect();
        let hops = ttls
            .into_iter()
            .map(|ttl| {
                let col: Vec<Option<&HopStat>> = stats.iter().map(|m| m.get(&ttl)).collect();
                let mut addrs: HashMap<&str, u32> = HashMap::new();
                for s in col.iter().flatten() {
                    if let Some(a) = &s.addr {
                        *addrs.entry(a).or_default() += s.recv;
                    }
                }
                HopSeries {
                    ttl,
                    addr: addrs
                        .into_iter()
                        .max_by_key(|(a, n)| (*n, std::cmp::Reverse(*a)))
                        .map(|(a, _)| a.to_string()),
                    loss_pct: col.iter().map(|s| s.map(|s| s.loss_pct)).collect(),
                    eff_loss_pct: col.iter().map(|s| s.map(|s| s.eff_loss_pct)).collect(),
                    rate_limited: col
                        .iter()
                        .map(|s| s.is_some_and(|s| s.rate_limited))
                        .collect(),
                    avg_ms: col.iter().map(|s| s.and_then(|s| s.avg_ms)).collect(),
                }
            })
            .collect();
        Ok(HopsResponse {
            step: g.step,
            ts: g.ts,
            hops,
        })
    })
    .await?;
    Ok(Json(out))
}

#[derive(Serialize)]
struct SliceHop {
    #[serde(flatten)]
    stat: HopStat,
    /// More than one responder answered at this TTL inside the window.
    addr_changed: bool,
}

#[derive(Serialize)]
struct SliceResponse {
    from: i64,
    to: i64,
    hops: Vec<SliceHop>,
}

async fn slice(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Params,
) -> ApiResult<SliceResponse> {
    let target = find_target(&state, &id)?;
    let (from, to) = range(&q, now())?;
    let out = blocking(move || {
        let table = pick_table(from, now(), 60, state.inner.minute_days);
        let step = if table == Table::Hour { 3600 } else { 60 };
        let rows = state
            .inner
            .store
            .hop_buckets(table, target.id, from, to, step)?;
        let mut seen: HashMap<u8, BTreeSet<&str>> = HashMap::new();
        for r in &rows {
            if let Some(a) = &r.addr {
                seen.entry(r.ttl).or_default().insert(a);
            }
        }
        let hops = analyze(&merge_by_ttl(&rows))
            .into_iter()
            .map(|stat| SliceHop {
                addr_changed: seen.get(&stat.ttl).is_some_and(|s| s.len() > 1),
                stat,
            })
            .collect();
        Ok(SliceResponse { from, to, hops })
    })
    .await?;
    Ok(Json(out))
}
