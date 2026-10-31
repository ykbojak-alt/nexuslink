//! Request metrics per (provider, model, key tag) — in-memory, uptime-scoped
//! like the blacklist: a restart is a clean slate. Keys appear only as the
//! masked tail tag, same rule as `rotation::key_tag`.

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::Instant;

#[derive(Debug, Default)]
pub struct Metrics {
    rows: RwLock<HashMap<String, Stat>>,
}

#[derive(Debug)]
struct Stat {
    ok: u64,
    fail: u64,
    /// EMA of attempt latency, ms — recent behavior weighs more than history.
    avg_ms: f64,
    last_ms: u64,
    last_note: String,
    last_at: Instant,
}

impl Metrics {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one candidate attempt. Scope: `provider|model|keytag`.
    pub fn record(&self, provider: &str, model: &str, api_key: &str, ok: bool,
                  latency_ms: u64, note: &str) {
        let key = format!("{provider}|{model}|{}", crate::rotation::key_tag(api_key));
        let mut rows = self.rows.write().unwrap();
        let s = rows.entry(key).or_insert(Stat {
            ok: 0,
            fail: 0,
            avg_ms: 0.0,
            last_ms: 0,
            last_note: String::new(),
            last_at: Instant::now(),
        });
        if ok {
            s.ok += 1;
        } else {
            s.fail += 1;
        }
        s.avg_ms = if s.avg_ms == 0.0 {
            latency_ms as f64
        } else {
            s.avg_ms * 0.8 + latency_ms as f64 * 0.2
        };
        s.last_ms = latency_ms;
        if !note.is_empty() {
            s.last_note = note.to_string();
        }
        s.last_at = Instant::now();
    }

    /// Snapshot rows for /api/status, most-active first:
    /// (scope, ok, fail, avg_ms, last_ms, last_note, age_s).
    pub fn snapshot(&self) -> Vec<(String, u64, u64, u64, u64, String, u64)> {
        let now = Instant::now();
        let mut v: Vec<_> = self
            .rows
            .read()
            .unwrap()
            .iter()
            .map(|(k, s)| {
                (
                    k.clone(),
                    s.ok,
                    s.fail,
                    s.avg_ms.round() as u64,
                    s.last_ms,
                    s.last_note.clone(),
                    now.duration_since(s.last_at).as_secs(),
                )
            })
            .collect();
        v.sort_by(|a, b| (b.1 + b.2).cmp(&(a.1 + a.2)));
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_ok_fail_and_masks_keys() {
        let m = Metrics::new();
        m.record("prov", "mod", "sk-secret-aaaaaa", true, 120, "");
        m.record("prov", "mod", "sk-secret-aaaaaa", false, 340, "HTTP 429");
        m.record("prov", "mod", "sk-other-bbbbbb", true, 90, "");
        let rows = m.snapshot();
        assert_eq!(rows.len(), 2);
        let (scope, ok, fail, _avg, last, note, _age) = &rows[0];
        assert_eq!(scope, "prov|mod|aaaaaa");
        assert_eq!(*ok, 1);
        assert_eq!(*fail, 1);
        assert!(*last > 0);
        assert_eq!(note, "HTTP 429");
        assert!(!scope.contains("sk-secret"));
    }
}
