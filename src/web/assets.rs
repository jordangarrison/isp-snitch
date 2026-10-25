//! Static UI files compiled into the binary.

use axum::extract::Path;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};

const INDEX: &str = include_str!("../../assets/index.html");
const TARGET: &str = include_str!("../../assets/target.html");

/// (published path under /assets/, content type, bytes)
const FILES: &[(&str, &str, &[u8])] = &[
    (
        "app.css",
        "text/css; charset=utf-8",
        include_bytes!("../../assets/app.css"),
    ),
    (
        "common.js",
        "text/javascript; charset=utf-8",
        include_bytes!("../../assets/common.js"),
    ),
    (
        "dash.css",
        "text/css; charset=utf-8",
        include_bytes!("../../assets/dash.css"),
    ),
    (
        "dash.js",
        "text/javascript; charset=utf-8",
        include_bytes!("../../assets/dash.js"),
    ),
    (
        "target.js",
        "text/javascript; charset=utf-8",
        include_bytes!("../../assets/target.js"),
    ),
    (
        "vendor/uplot/uPlot.iife.min.js",
        "text/javascript; charset=utf-8",
        include_bytes!("../../assets/vendor/uplot/uPlot.iife.min.js"),
    ),
    (
        "vendor/uplot/uPlot.min.css",
        "text/css; charset=utf-8",
        include_bytes!("../../assets/vendor/uplot/uPlot.min.css"),
    ),
];

fn html(body: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}

pub async fn index() -> Response {
    html(INDEX)
}

pub async fn target_page() -> Response {
    html(TARGET)
}

pub async fn asset(Path(path): Path<String>) -> Response {
    match FILES.iter().find(|(p, _, _)| *p == path) {
        Some((_, ctype, bytes)) => (
            [
                (header::CONTENT_TYPE, *ctype),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            *bytes,
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}
