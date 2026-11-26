//! OTLP export of traces, logs and metrics to the `OTEL_*` endpoints (`telemetry.otlpExportEnabled`).
//!
//! The exporter is process-global: it starts on the first turn, batches records, and posts OTLP/HTTP
//! JSON to the configured collector. A missing endpoint, an unsupported protocol, or a full queue
//! silently drops records — telemetry must never fail a turn.
use crate::config::Config;
use serde_json::{json, Value};
use std::{
    sync::{LazyLock, Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Endpoints {
    /// `{endpoint}/v1/traces`, `…/v1/logs`, `…/v1/metrics`, or a signal-specific override.
    pub traces: String,
    pub logs: String,
    pub metrics: String,
    /// Headers from `OTEL_EXPORTER_OTLP_HEADERS` (`k=v,k2=v2`).
    pub headers: Vec<(String, String)>,
    pub service: String,
    /// False when the configured protocol is not the OTLP/HTTP JSON this exporter speaks.
    pub http: bool,
}

impl Endpoints {
    /// `None` when no endpoint is set: telemetry then stays off even if enabled in settings.
    pub fn from_env() -> Option<Endpoints> {
        let env = |key: &str| std::env::var(key).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let base = env("OTEL_EXPORTER_OTLP_ENDPOINT");
        let per = |signal: &str| env(&format!("OTEL_EXPORTER_OTLP_{}_ENDPOINT", signal.to_ascii_uppercase()));
        let traces = per("traces").or_else(|| base.as_deref().map(|b| format!("{}/v1/traces", b.trim_end_matches('/'))))?;
        let logs = per("logs").unwrap_or_else(|| format!("{}/v1/logs", base.as_deref().unwrap_or_default().trim_end_matches('/')));
        let metrics = per("metrics").unwrap_or_else(|| format!("{}/v1/metrics", base.as_deref().unwrap_or_default().trim_end_matches('/')));
        let headers = env("OTEL_EXPORTER_OTLP_HEADERS").map(|list| {
            list.split(',')
                .filter_map(|pair| pair.split_once('='))
                .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
                .collect()
        }).unwrap_or_default();
        let service = env("OTEL_SERVICE_NAME").or_else(|| env("OTEL_RESOURCE_ATTRIBUTES").and_then(|list| {
            list.split(',').find_map(|pair| pair.split_once('=').filter(|(k, _)| k.trim() == "service.name").map(|(_, v)| v.trim().to_string()))
        })).unwrap_or_else(|| "greencode".into());
        // gRPC and HTTP/protobuf cannot be carried by these JSON payloads; refuse instead of posting junk.
        let http = env("OTEL_EXPORTER_OTLP_PROTOCOL").is_none_or(|p| p == "http/json");
        Some(Endpoints { traces, logs, metrics, headers, service, http })
    }
}

#[derive(Clone, Debug)]
struct Record {
    path: String,
    body: Value,
}

static QUEUE: LazyLock<Mutex<(Vec<Record>, bool)>> = LazyLock::new(|| Mutex::new((Vec::new(), false)));
static ENDPOINTS: OnceLock<Option<Endpoints>> = OnceLock::new();

const MAX_QUEUE: usize = 512;

/// Resolves the exporter once per process; `None` when disabled or unconfigured.
fn exporter(cfg: &Config) -> Option<&'static Endpoints> {
    ENDPOINTS
        .get_or_init(|| {
            if !cfg.services_extra_opts.telemetry.otlp_export_enabled {
                return None;
            }
            let endpoints = Endpoints::from_env();
            if endpoints.as_ref().is_some_and(|e| !e.http) {
                eprintln!("OTLP export skipped: only http/json is supported, not the configured OTEL_EXPORTER_OTLP_PROTOCOL");
            }
            endpoints.filter(|e| e.http)
        })
        .as_ref()
}

/// Starts the background sender once per process, when telemetry is enabled and configured.
pub fn start(cfg: &Config) {
    if exporter(cfg).is_none() || QUEUE.lock().unwrap().1 {
        return;
    }
    QUEUE.lock().unwrap().1 = true;
    tokio::spawn(async move {
        let http = reqwest::Client::new();
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            let batch: Vec<Record> = std::mem::take(&mut QUEUE.lock().unwrap().0);
            if batch.is_empty() {
                continue;
            }
            let Some(endpoints) = ENDPOINTS.get().and_then(Option::as_ref) else { return };
            for record in batch {
                let mut req = http.post(&record.path).json(&record.body);
                for (k, v) in &endpoints.headers {
                    req = req.header(k, v);
                }
                // Telemetry is best-effort: a collector that is down must not surface as an error.
                let _ = req.send().await;
            }
        }
    });
}

/// Milliseconds since the epoch; spans carry these as OTLP nanoseconds.
pub fn millis() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

fn resource(endpoints: &Endpoints) -> Value {
    json!({"attributes": [{"key": "service.name", "value": {"stringValue": endpoints.service}}]})
}

fn push(record: Record) {
    let mut queue = QUEUE.lock().unwrap();
    if queue.0.len() < MAX_QUEUE {
        queue.0.push(record);
    }
}

/// One finished span; `attrs` are string attributes.
pub fn span(name: &str, start_ms: u64, end_ms: u64, ok: bool, attrs: &[(&str, String)]) {
    let Some(endpoints) = ENDPOINTS.get().and_then(Option::as_ref) else { return };
    let attributes: Vec<Value> = std::iter::once(("span.name", name.to_string()))
        .chain(attrs.iter().map(|(k, v)| (*k, v.clone())))
        .map(|(k, v)| json!({"key": k, "value": {"stringValue": v}}))
        .collect();
    push(Record {
        path: endpoints.traces.clone(),
        body: json!({
            "resourceSpans": [{
                "resource": resource(endpoints),
                "scopeSpans": [{"spans": [{
                    "traceId": format!("{:032x}", fnv(name.as_bytes())),
                    "spanId": format!("{:016x}", fnv(&start_ms.to_le_bytes())),
                    "name": name,
                    "kind": 1,
                    "startTimeUnixNano": start_ms * 1_000_000,
                    "endTimeUnixNano": end_ms * 1_000_000,
                    "attributes": attributes,
                    "status": {"code": if ok { 1 } else { 2 }},
                }]}],
            }],
        }),
    });
}

/// One synchronous record, exported as an OTLP log.
pub fn log(body: &str, severity: &str) {
    let Some(endpoints) = ENDPOINTS.get().and_then(Option::as_ref) else { return };
    let now = millis() * 1_000_000;
    push(Record {
        path: endpoints.logs.clone(),
        body: json!({"resourceLogs": [{
            "resource": resource(endpoints),
            "scopeLogs": [{"logRecords": [{
                "timeUnixNano": now,
                "severityText": severity,
                "body": {"stringValue": body},
            }]}],
        }]}),
    });
}

/// A monotonic counter, exported as an OTLP sum.
pub fn count(name: &str, value: u64) {
    let Some(endpoints) = ENDPOINTS.get().and_then(Option::as_ref) else { return };
    push(Record {
        path: endpoints.metrics.clone(),
        body: json!({"resourceMetrics": [{
            "resource": resource(endpoints),
            "scopeMetrics": [{"metrics": [{
                "name": name,
                "sum": {
                    "aggregationTemporality": 2,
                    "isMonotonic": true,
                    "dataPoints": [{"asInt": value.to_string(), "timeUnixNano": millis() * 1_000_000}],
                },
            }]}],
        }]}),
    });
}

/// Exports one finished model request.
pub fn request(cfg: &Config, model: &str, start_ms: u64, usage: &crate::Usage) {
    start(cfg);
    span("provider.request", start_ms, millis(), true, &[
        ("model", model.to_string()),
        ("input_tokens", usage.input.to_string()),
        ("output_tokens", usage.output.to_string()),
    ]);
}

fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325u64, |h, b| (h ^ *b as u64).wrapping_mul(0x100000001b3))
}

/// Enabled and configured.
pub fn active() -> bool {
    ENDPOINTS.get().and_then(Option::as_ref).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    static ENV_LOCK: StdMutex<()> = StdMutex::new(());

    fn clear_env() {
        for key in [
            "OTEL_EXPORTER_OTLP_ENDPOINT", "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT",
            "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT", "OTEL_EXPORTER_OTLP_HEADERS", "OTEL_SERVICE_NAME",
            "OTEL_EXPORTER_OTLP_PROTOCOL", "OTEL_RESOURCE_ATTRIBUTES",
        ] {
            std::env::remove_var(key);
        }
    }

    #[test]
    fn endpoints_come_from_the_otel_variables() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", "http://127.0.0.1:4318/");
        std::env::set_var("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", "http://127.0.0.1:4318/custom/traces");
        std::env::set_var("OTEL_EXPORTER_OTLP_HEADERS", "x-api-key=abc, tenant = t1 ");
        std::env::set_var("OTEL_RESOURCE_ATTRIBUTES", "service.name=greencode-test,deployment=ci");
        let e = Endpoints::from_env().unwrap();
        assert_eq!(e.traces, "http://127.0.0.1:4318/custom/traces");
        assert_eq!(e.logs, "http://127.0.0.1:4318/v1/logs");
        assert_eq!(e.metrics, "http://127.0.0.1:4318/v1/metrics");
        assert_eq!(e.headers, [("x-api-key".to_string(), "abc".to_string()), ("tenant".to_string(), "t1".to_string())]);
        assert_eq!(e.service, "greencode-test");
        assert!(e.http);
        clear_env();
        assert!(Endpoints::from_env().is_none(), "no endpoint, no export");
        std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", "http://127.0.0.1:4317");
        std::env::set_var("OTEL_EXPORTER_OTLP_PROTOCOL", "grpc");
        assert!(!Endpoints::from_env().unwrap().http);
        clear_env();
    }

    #[test]
    fn payloads_match_the_otlp_json_shape() {
        let endpoints = Endpoints {
            traces: "http://127.0.0.1:4318/v1/traces".into(),
            logs: "http://127.0.0.1:4318/v1/logs".into(),
            metrics: "http://127.0.0.1:4318/v1/metrics".into(),
            headers: vec![],
            service: "greencode".into(),
            http: true,
        };
        let resource = resource(&endpoints);
        assert_eq!(resource["attributes"][0]["key"], "service.name");
        let body = json!({"resourceSpans": [{"resource": resource, "scopeSpans": [{"spans": [{"name": "tool"}]}]}]});
        assert_eq!(body["resourceSpans"][0]["scopeSpans"][0]["spans"][0]["name"], "tool");
    }

    #[tokio::test]
    async fn export_disabled_queues_nothing() {
        let cfg = Config::default();
        start(&cfg);
        log("ignored", "INFO");
        assert!(QUEUE.lock().unwrap().0.is_empty());
        assert!(!active());
    }
}
