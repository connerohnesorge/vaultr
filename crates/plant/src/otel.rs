//! OTLP telemetry — ports wireproxy.ts:184-355. Same metric names, bounds, shapes.

use crate::adapter::Adapter;
use crate::capture::{CapturedRequest, CapturedResponse};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

pub const HISTOGRAM_BOUNDS: [u64; 11] = [
    100, 250, 500, 1_000, 2_500, 5_000, 10_000, 30_000, 60_000, 120_000, 300_000,
];

struct HistogramPoint {
    attributes: Value,
    count: u64,
    sum: u64,
    buckets: Vec<u64>,
}

struct State {
    tokens: HashMap<String, (Value, u64)>,
    requests: HashMap<String, (Value, u64)>,
    durations: HashMap<String, HistogramPoint>,
    logs: Vec<Value>,
}

pub struct Otel {
    pub enabled: bool,
    pub endpoint: String,
    start_ns: String,
    state: Mutex<State>,
}

fn now_ns() -> String {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}", d.as_millis() * 1_000_000)
}

fn attributes(values: &[(&str, Value)]) -> Value {
    Value::Array(
        values
            .iter()
            .map(|(key, value)| {
                let v = match value {
                    Value::String(s) => json!({ "stringValue": s }),
                    Value::Bool(b) => json!({ "boolValue": b }),
                    Value::Number(n) => json!({ "intValue": n.to_string() }),
                    other => json!({ "stringValue": other.to_string() }),
                };
                json!({ "key": key, "value": v })
            })
            .collect(),
    )
}

fn attrs_from_map(map: &Map<String, Value>) -> Value {
    attributes(
        &map.iter()
            .map(|(k, v)| (k.as_str(), v.clone()))
            .collect::<Vec<_>>(),
    )
}

fn resource(loki_labels: bool) -> Value {
    let mut vals: Vec<(&str, Value)> = vec![
        ("service.namespace", json!("claude-code")),
        ("service.name", json!("cohnesor")),
        ("deployment.environment", json!("workstation")),
        ("host.name", json!(hostname())),
        ("service.instance.id", json!("vaultr")),
    ];
    if loki_labels {
        vals.push(("loki.resource.labels", json!("service.namespace")));
    }
    json!({ "attributes": attributes(&vals) })
}

fn hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".into())
}

impl Otel {
    pub fn new() -> Self {
        Otel {
            enabled: std::env::var("VAULTR_OTEL").as_deref() != Ok("0"),
            endpoint: std::env::var("VAULTR_OTEL_ENDPOINT")
                .unwrap_or_else(|_| "https://otlp.lan.cnb.rocks".into())
                .trim_end_matches('/')
                .to_string(),
            start_ns: now_ns(),
            state: Mutex::new(State {
                tokens: HashMap::new(),
                requests: HashMap::new(),
                durations: HashMap::new(),
                logs: vec![],
            }),
        }
    }

    pub fn record(
        &self,
        adapter: &Adapter,
        model: Option<&str>,
        req: &CapturedRequest,
        resp: &CapturedResponse,
    ) {
        if !self.enabled {
            return;
        }
        let model = model.unwrap_or("unknown").to_string();
        let usage = adapter.usage(&vaultr::recon::parse_sse(&resp.sse));
        let duration_ms = req
            .started_at
            .elapsed()
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        let mut st = self.state.lock().unwrap();
        for (typ, value) in [
            ("input", usage.input),
            ("output", usage.output),
            ("cache_read", usage.cache_read),
            ("cache_creation", usage.cache_creation),
        ] {
            if value > 0 {
                let attrs = json!({ "type": typ, "model": model, "harness": adapter.harness });
                let key = attrs.to_string();
                st.tokens.entry(key).or_insert((attrs, 0)).1 += value;
            }
        }
        let req_attrs = json!({
            "status": resp.status.to_string(), "model": model, "harness": adapter.harness,
            "complete": resp.complete,
        });
        st.requests
            .entry(req_attrs.to_string())
            .or_insert((req_attrs, 0))
            .1 += 1;

        let common = json!({ "model": model, "harness": adapter.harness });
        let key = common.to_string();
        let hist = st.durations.entry(key).or_insert_with(|| HistogramPoint {
            attributes: common,
            count: 0,
            sum: 0,
            buckets: vec![0; HISTOGRAM_BOUNDS.len() + 1],
        });
        hist.count += 1;
        hist.sum += duration_ms;
        let bucket = HISTOGRAM_BOUNDS
            .iter()
            .position(|&b| duration_ms <= b)
            .unwrap_or(HISTOGRAM_BOUNDS.len());
        hist.buckets[bucket] += 1;

        let ok = resp.status < 400 && resp.complete;
        let t = now_ns();
        let log_attrs = attributes(&[
            (
                "session_id",
                json!(req.ids.session_id.as_deref().unwrap_or("unknown")),
            ),
            ("model", json!(model)),
            ("harness", json!(adapter.harness)),
            ("status", json!(resp.status)),
            ("tokens", json!(usage.input + usage.output)),
            ("duration_ms", json!(duration_ms)),
        ]);
        st.logs.push(json!({
            "timeUnixNano": t,
            "observedTimeUnixNano": t,
            "severityNumber": if ok { 9 } else { 13 },
            "severityText": if ok { "INFO" } else { "WARN" },
            "body": { "stringValue": format!("{} {} request {}{}", adapter.harness, model, resp.status, if resp.complete { "" } else { " incomplete" }) },
            "attributes": log_attrs,
        }));
        // ponytail: bound outage memory; dropped logs remain recoverable from turns.jsonl.
        let len = st.logs.len();
        if len > 1_000 {
            st.logs.drain(0..len - 1_000);
        }
    }

    fn counter_metric(&self, name: &str, points: &HashMap<String, (Value, u64)>) -> Value {
        json!({
            "name": name,
            "sum": {
                "aggregationTemporality": 2,
                "isMonotonic": true,
                "dataPoints": points.values().map(|(attrs, value)| json!({
                    "attributes": attrs_from_map(attrs.as_object().unwrap()),
                    "startTimeUnixNano": self.start_ns,
                    "timeUnixNano": now_ns(),
                    "asInt": value.to_string(),
                })).collect::<Vec<_>>(),
            },
        })
    }

    fn metrics_payload(&self, st: &State) -> Value {
        let t = now_ns();
        let duration_points: Vec<Value> = st
            .durations
            .values()
            .map(|p| {
                json!({
                    "attributes": attrs_from_map(p.attributes.as_object().unwrap()),
                    "startTimeUnixNano": self.start_ns,
                    "timeUnixNano": t,
                    "count": p.count.to_string(),
                    "sum": p.sum,
                    "explicitBounds": HISTOGRAM_BOUNDS,
                    "bucketCounts": p.buckets.iter().map(|b| b.to_string()).collect::<Vec<_>>(),
                })
            })
            .collect();
        let metrics = vec![
            self.counter_metric("vaultr.tokens", &st.tokens),
            self.counter_metric("vaultr.requests", &st.requests),
            json!({
                "name": "vaultr.request.duration",
                "unit": "ms",
                "histogram": { "aggregationTemporality": 2, "dataPoints": duration_points },
            }),
        ];
        json!({ "resourceMetrics": [{
            "resource": resource(false),
            "scopeMetrics": [{ "scope": { "name": "vaultr" }, "metrics": metrics }],
        }] })
    }

    pub async fn flush(&self, client: &reqwest::Client, token_override: Option<&str>) {
        if !self.enabled && token_override.is_none() {
            return;
        }
        let token = match token_override {
            Some(t) => t.to_string(),
            None => {
                let out = tokio::process::Command::new("cnb")
                    .args(["auth", "token"])
                    .env("PATH", crate::process::augmented_path())
                    .output()
                    .await;
                match out {
                    Ok(o) if o.status.success() => {
                        String::from_utf8_lossy(&o.stdout).trim().to_string()
                    }
                    _ => {
                        eprintln!("[otel] cnb auth token failed; skipping flush");
                        return;
                    }
                }
            }
        };
        if token.is_empty() {
            eprintln!("[otel] cnb auth token failed; skipping flush");
            return;
        }
        // Drain logs before export (matches TS splice(0): failed exports drop logs)
        let (metrics, logs) = {
            let mut st = self.state.lock().unwrap();
            let records: Vec<Value> = st.logs.drain(..).collect();
            let metrics = self.metrics_payload(&st);
            let logs = json!({ "resourceLogs": [{
                "resource": resource(true),
                "scopeLogs": [{ "scope": { "name": "vaultr" }, "logRecords": records }],
            }] });
            (metrics, logs)
        };
        for (path, body) in [("/v1/metrics", metrics), ("/v1/logs", logs)] {
            let res = client
                .post(format!("{}{}", self.endpoint, path))
                .header("content-type", "application/json")
                .bearer_auth(&token)
                .json(&body)
                .send()
                .await;
            match res {
                Ok(r) if r.status().is_success() => {}
                Ok(r) => eprintln!("[otel] export failed: {path}: {}", r.status()),
                Err(e) => eprintln!("[otel] export failed: {e}"),
            }
        }
    }
}
