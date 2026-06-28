//! Minimal OTLP/HTTP+JSON decoding for Claude Code's `api_request` log events.
//!
//! Claude Code's native OpenTelemetry emits one `api_request` **log record** per
//! API call, carrying the model and token counts as record attributes. We parse
//! the OTLP/JSON logs envelope (`resourceLogs → scopeLogs → logRecords`) directly
//! rather than pulling the full protobuf stack: the bootstrap (`18`) points the
//! exporter at this receiver with `OTEL_EXPORTER_OTLP_PROTOCOL=http/json`.
//!
//! Using the per-request **log event** as the turn unit (instead of summing
//! metric counters) sidesteps OTEL's delta-vs-cumulative temporality entirely —
//! a short session's single event is never lost to an unflushed counter.

use serde::Deserialize;

use crate::model::{RawTurn, Source, HARNESS_CLAUDE_CODE_CLI};

/// A number that may arrive as a JSON number or, per OTLP/JSON's int64 rule, a
/// quoted string.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum JsonNum {
    Int(i64),
    Str(String),
}

impl JsonNum {
    fn as_i64(&self) -> Option<i64> {
        match self {
            JsonNum::Int(i) => Some(*i),
            JsonNum::Str(s) => s.parse().ok(),
        }
    }
}

/// An OTLP `AnyValue` — only the scalar variants we read are modeled.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct AnyValue {
    string_value: Option<String>,
    int_value: Option<JsonNum>,
    double_value: Option<f64>,
    bool_value: Option<bool>,
}

impl AnyValue {
    fn as_u64(&self) -> Option<u64> {
        if let Some(i) = self.int_value.as_ref().and_then(JsonNum::as_i64) {
            return u64::try_from(i).ok();
        }
        if let Some(d) = self.double_value {
            if d >= 0.0 {
                return Some(d as u64);
            }
        }
        self.string_value.as_ref().and_then(|s| s.parse().ok())
    }

    fn as_f64(&self) -> Option<f64> {
        self.double_value
            .or_else(|| {
                self.int_value
                    .as_ref()
                    .and_then(JsonNum::as_i64)
                    .map(|i| i as f64)
            })
            .or_else(|| self.string_value.as_ref().and_then(|s| s.parse().ok()))
    }

    fn as_str(&self) -> Option<&str> {
        self.string_value.as_deref()
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct KeyValue {
    key: String,
    value: Option<AnyValue>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Resource {
    attributes: Vec<KeyValue>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct LogRecord {
    time_unix_nano: Option<JsonNum>,
    body: Option<AnyValue>,
    attributes: Vec<KeyValue>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct ScopeLogs {
    log_records: Vec<LogRecord>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct ResourceLogs {
    resource: Option<Resource>,
    scope_logs: Vec<ScopeLogs>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct ExportLogsServiceRequest {
    resource_logs: Vec<ResourceLogs>,
}

fn find<'a>(attrs: &'a [KeyValue], keys: &[&str]) -> Option<&'a AnyValue> {
    keys.iter().find_map(|k| {
        attrs
            .iter()
            .find(|kv| kv.key == *k)
            .and_then(|kv| kv.value.as_ref())
    })
}

/// Is this log record Claude Code's `api_request` event (the one carrying token
/// usage)? Matched on the record body or an `event.name` attribute.
fn is_api_request(record: &LogRecord) -> bool {
    let body_is = record
        .body
        .as_ref()
        .and_then(AnyValue::as_str)
        .map(|s| s == "api_request")
        .unwrap_or(false);
    let name_is = find(&record.attributes, &["event.name", "event_name", "name"])
        .and_then(AnyValue::as_str)
        .map(|s| s == "api_request")
        .unwrap_or(false);
    body_is || name_is
}

/// Decode an OTLP/JSON `ExportLogsServiceRequest` into raw turns, one per
/// `api_request` event. `fallback_ts` stamps records missing a timestamp.
pub fn turns_from_logs_json(
    bytes: &[u8],
    fallback_ts: i64,
) -> Result<Vec<RawTurn>, serde_json::Error> {
    let req: ExportLogsServiceRequest = serde_json::from_slice(bytes)?;
    let mut turns = Vec::new();

    for resource_logs in &req.resource_logs {
        let resource_attrs = resource_logs
            .resource
            .as_ref()
            .map(|r| r.attributes.as_slice())
            .unwrap_or(&[]);

        for scope in &resource_logs.scope_logs {
            for record in &scope.log_records {
                if !is_api_request(record) {
                    continue;
                }
                turns.push(record_to_turn(record, resource_attrs, fallback_ts));
            }
        }
    }
    Ok(turns)
}

fn record_to_turn(record: &LogRecord, resource_attrs: &[KeyValue], fallback_ts: i64) -> RawTurn {
    // Record attributes win over resource attributes for the same key.
    let attr =
        |keys: &[&str]| find(&record.attributes, keys).or_else(|| find(resource_attrs, keys));

    let tokens = |keys: &[&str]| attr(keys).and_then(AnyValue::as_u64).unwrap_or(0);
    let cache = tokens(&["cache_read_tokens", "cache_read_input_tokens"])
        + tokens(&["cache_creation_tokens", "cache_creation_input_tokens"]);

    let timestamp_ms = record
        .time_unix_nano
        .as_ref()
        .and_then(JsonNum::as_i64)
        .map(|nanos| nanos / 1_000_000)
        .unwrap_or(fallback_ts);

    RawTurn {
        source: Source::Otel,
        model: attr(&["model"])
            .and_then(AnyValue::as_str)
            .map(str::to_string),
        harness: HARNESS_CLAUDE_CODE_CLI.to_string(),
        tokens_input: tokens(&["input_tokens"]),
        tokens_output: tokens(&["output_tokens"]),
        // OTEL bills thinking inside output and never breaks it out.
        tokens_thinking: 0,
        tokens_cache: cache,
        prompt_id: attr(&["prompt.id", "prompt_id"])
            .and_then(AnyValue::as_str)
            .map(str::to_string),
        timestamp_ms,
        cost_usd: attr(&["cost_usd"]).and_then(AnyValue::as_f64),
        duration_ms: attr(&["duration_ms"]).and_then(AnyValue::as_u64),
        session_id: attr(&["session.id", "session_id"])
            .and_then(AnyValue::as_str)
            .map(str::to_string),
        workspace: attr(&["cwd", "workspace", "terminal.cwd"])
            .and_then(AnyValue::as_str)
            .map(str::to_string),
        // OTEL always reports real token counts.
        counts_estimated: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A realistic Claude Code OTLP/JSON logs export (int64s as quoted strings).
    const SAMPLE: &str = r#"{
      "resourceLogs": [{
        "resource": { "attributes": [
          { "key": "session.id", "value": { "stringValue": "sess-1" } }
        ]},
        "scopeLogs": [{
          "logRecords": [
            {
              "timeUnixNano": "1781636400000000000",
              "body": { "stringValue": "api_request" },
              "attributes": [
                { "key": "model", "value": { "stringValue": "claude-opus-4-8" } },
                { "key": "input_tokens", "value": { "intValue": "120" } },
                { "key": "output_tokens", "value": { "intValue": "340" } },
                { "key": "cache_read_tokens", "value": { "intValue": "40" } },
                { "key": "cache_creation_tokens", "value": { "intValue": "10" } },
                { "key": "cost_usd", "value": { "doubleValue": 0.0123 } },
                { "key": "duration_ms", "value": { "intValue": "1500" } },
                { "key": "prompt.id", "value": { "stringValue": "p-42" } }
              ]
            },
            {
              "body": { "stringValue": "tool_result" },
              "attributes": [ { "key": "output_tokens", "value": { "intValue": "999" } } ]
            }
          ]
        }]
      }]
    }"#;

    #[test]
    fn extracts_one_turn_from_the_api_request_event_only() {
        let turns = turns_from_logs_json(SAMPLE.as_bytes(), 0).unwrap();
        assert_eq!(turns.len(), 1, "tool_result is ignored");
        let t = &turns[0];
        assert_eq!(t.source, Source::Otel);
        assert_eq!(t.model.as_deref(), Some("claude-opus-4-8"));
        assert_eq!(t.tokens_input, 120);
        assert_eq!(t.tokens_output, 340);
        assert_eq!(t.tokens_cache, 50, "read + creation");
        assert_eq!(t.tokens_thinking, 0, "OTEL does not break thinking out");
        assert_eq!(t.cost_usd, Some(0.0123));
        assert_eq!(t.duration_ms, Some(1500));
        assert_eq!(t.prompt_id.as_deref(), Some("p-42"));
        assert_eq!(
            t.session_id.as_deref(),
            Some("sess-1"),
            "from resource attrs"
        );
        assert_eq!(t.timestamp_ms, 1_781_636_400_000);
    }

    #[test]
    fn empty_or_unrelated_payloads_yield_no_turns() {
        assert!(turns_from_logs_json(b"{}", 0).unwrap().is_empty());
        assert!(turns_from_logs_json(br#"{"resourceLogs":[]}"#, 0)
            .unwrap()
            .is_empty());
        assert!(turns_from_logs_json(b"not json", 0).is_err());
    }

    #[test]
    fn tolerates_numeric_int_values() {
        // Some exporters emit intValue as a JSON number rather than a string.
        let json = r#"{"resourceLogs":[{"scopeLogs":[{"logRecords":[
          {"body":{"stringValue":"api_request"},"attributes":[
            {"key":"model","value":{"stringValue":"m"}},
            {"key":"output_tokens","value":{"intValue":7}}]}]}]}]}"#;
        let turns = turns_from_logs_json(json.as_bytes(), 0).unwrap();
        assert_eq!(turns[0].tokens_output, 7);
    }
}
