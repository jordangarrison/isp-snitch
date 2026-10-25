//! One wide event per HTTP request.

use crate::wide::{Event, Level};
use axum::extract::Request;
use axum::http::header::USER_AGENT;
use axum::middleware::Next;
use axum::response::Response;
use serde_json::json;
use std::time::Instant;

/// Builds the request event, or `None` for paths that are never logged.
pub fn http_event(
    method: &str,
    path: &str,
    status: u16,
    duration_ms: u64,
    user_agent: Option<&str>,
) -> Option<Event> {
    if path == "/healthz" {
        return None;
    }
    let (outcome, level) = match status {
        500.. => ("error", Level::Error),
        400..=499 => ("client_error", Level::Warn),
        _ => ("success", Level::Info),
    };
    let mut ev = Event::new("http_request");
    ev.level(level)
        .set("method", method)
        .set("path", path)
        .set("status", status)
        .set("duration_ms", duration_ms)
        .set("outcome", outcome)
        .set("client", json!({ "user_agent": user_agent }));
    Some(ev)
}

pub async fn http_wide_event(req: Request, next: Next) -> Response {
    let started = Instant::now();
    let method = req.method().to_string();
    let path = req.uri().path().to_string();
    let ua = req
        .headers()
        .get(USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let resp = next.run(req).await;
    if let Some(ev) = http_event(
        &method,
        &path,
        resp.status().as_u16(),
        started.elapsed().as_millis() as u64,
        ua.as_deref(),
    ) {
        ev.emit();
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(path: &str, status: u16) -> Option<serde_json::Value> {
        http_event("GET", path, status, 12, Some("curl/8"))
            .map(|e| serde_json::from_str(&e.to_line()).unwrap())
    }

    #[test]
    fn healthz_is_not_logged() {
        assert!(line("/healthz", 200).is_none());
    }

    #[test]
    fn success_event_has_canonical_fields() {
        let v = line("/api/targets", 200).unwrap();
        assert_eq!(v["action"], "http_request");
        assert_eq!(v["method"], "GET");
        assert_eq!(v["path"], "/api/targets");
        assert_eq!(v["status"], 200);
        assert_eq!(v["duration_ms"], 12);
        assert_eq!(v["outcome"], "success");
        assert_eq!(v["level"], "info");
        assert_eq!(v["client"]["user_agent"], "curl/8");
    }

    #[test]
    fn client_error_is_warn() {
        let v = line("/api/targets/9/hops", 404).unwrap();
        assert_eq!(v["outcome"], "client_error");
        assert_eq!(v["level"], "warn");
    }

    #[test]
    fn server_error_is_error() {
        let v = line("/api/targets", 500).unwrap();
        assert_eq!(v["outcome"], "error");
        assert_eq!(v["level"], "error");
    }
}
