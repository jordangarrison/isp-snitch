//! Wide-event logger: one JSON line per unit of work, `snake_case` fields.
//! Format rules live in the Projects/personal wide-event-logging standard.

use serde::Serialize;
use serde_json::{Map, Value};
use std::io::Write;
use std::time::SystemTime;

pub const SERVICE: &str = "isp-snitch";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Debug,
    Info,
    Warn,
    Error,
}

impl Level {
    fn as_str(self) -> &'static str {
        match self {
            Level::Debug => "debug",
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Error => "error",
        }
    }
}

pub struct Event {
    action: String,
    level: Level,
    fields: Map<String, Value>,
}

impl Event {
    pub fn new(action: &str) -> Self {
        Self {
            action: action.to_string(),
            level: Level::Info,
            fields: Map::new(),
        }
    }

    pub fn set(&mut self, key: &str, value: impl Serialize) -> &mut Self {
        let v = serde_json::to_value(value).unwrap_or(Value::Null);
        self.fields.insert(key.to_string(), v);
        self
    }

    pub fn level(&mut self, level: Level) -> &mut Self {
        self.level = level;
        self
    }

    pub fn to_line(&self) -> String {
        let mut m = Map::new();
        m.insert(
            "ts".into(),
            humantime::format_rfc3339_millis(SystemTime::now())
                .to_string()
                .into(),
        );
        m.insert("level".into(), self.level.as_str().into());
        m.insert("service".into(), SERVICE.into());
        m.insert("environment".into(), environment().into());
        m.insert("action".into(), self.action.clone().into());
        for (k, v) in &self.fields {
            m.insert(k.clone(), v.clone());
        }
        Value::Object(m).to_string()
    }

    pub fn emit(&self) {
        let line = self.to_line();
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{line}");
    }
}

/// Deployment environment, from `ISP_SNITCH_ENV` (default `production`).
fn environment() -> String {
    std::env::var("ISP_SNITCH_ENV").unwrap_or_else(|_| "production".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_has_canonical_fields() {
        let mut ev = Event::new("x");
        ev.set("post_count", 3);
        let v: serde_json::Value = serde_json::from_str(&ev.to_line()).unwrap();
        assert_eq!(v["service"], "isp-snitch");
        assert_eq!(v["action"], "x");
        assert_eq!(v["level"], "info");
        assert_eq!(v["post_count"], 3);
        assert!(v["ts"].as_str().unwrap().ends_with('Z'));
    }

    #[test]
    fn level_can_be_raised() {
        let mut ev = Event::new("x");
        ev.level(Level::Error);
        let v: serde_json::Value = serde_json::from_str(&ev.to_line()).unwrap();
        assert_eq!(v["level"], "error");
    }
}
