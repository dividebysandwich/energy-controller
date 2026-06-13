//! Model Context Protocol (MCP) server exposing the energy controller's
//! historical and current energy data to MCP-capable clients (e.g. LLM agents).
//!
//! Transport: the MCP "HTTP + Server-Sent Events" transport (protocol revision
//! 2024-11-05). A client opens a long-lived SSE stream on `GET <base>/mcp/sse`,
//! receives an `endpoint` event telling it where to POST, and then sends
//! JSON-RPC 2.0 requests to `POST <base>/mcp/messages?sessionId=...`. Each
//! request is answered asynchronously as a `message` event on the SSE stream.
//!
//! All tool names, argument names and result field names are intentionally
//! verbose and self-describing (including physical units and sign conventions)
//! so that a client with no prior knowledge of this code base can understand
//! exactly what every value means.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::{
    Router,
    extract::{Query, State},
    http::StatusCode,
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{get, post},
};
use chrono::{DateTime, Utc};
use futures_util::stream::{self, Stream};
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};

use crate::AppState;

/// Server identity reported to clients during the MCP `initialize` handshake.
const MCP_SERVER_NAME: &str = "energy-controller";
const MCP_SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
/// MCP protocol revision this server speaks (the HTTP+SSE transport revision).
const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

/// One selectable time-series channel of the rolling 24-hour energy history.
/// `key` is the value clients pass to the history tool; `description` and
/// `unit` are surfaced verbatim to the client so the meaning is unambiguous.
struct HistoryMetric {
    key: &'static str,
    unit: &'static str,
    description: &'static str,
}

const HISTORY_METRICS: &[HistoryMetric] = &[
    HistoryMetric {
        key: "battery_state_of_charge_percent",
        unit: "percent",
        description: "Battery state of charge (0-100%).",
    },
    HistoryMetric {
        key: "solar_pv_production_kw",
        unit: "kW",
        description: "Instantaneous photovoltaic (solar) production power.",
    },
    HistoryMetric {
        key: "household_consumption_load_kw",
        unit: "kW",
        description: "Instantaneous household electrical consumption (load).",
    },
    HistoryMetric {
        key: "grid_power_kw",
        unit: "kW",
        description:
            "Instantaneous grid power. Positive = importing from the grid, negative = exporting to the grid.",
    },
    HistoryMetric {
        key: "battery_power_kw",
        unit: "kW",
        description:
            "Instantaneous battery power. Positive = discharging (supplying the house), negative = charging.",
    },
];

/// Shared state for the MCP endpoints: a read handle on the latest controller
/// snapshot, the set of open SSE sessions, and the externally-visible base path.
#[derive(Clone)]
struct McpState {
    app_rx: watch::Receiver<Option<AppState>>,
    sessions: Arc<Mutex<HashMap<String, mpsc::UnboundedSender<Event>>>>,
    base_path: String,
}

/// Builds a fully self-contained (`Router<()>`) router exposing the MCP server
/// under `<base_path>/mcp/sse` and `<base_path>/mcp/messages`. Merge it into the
/// main web router. `base_path` is the deployment context path (may be empty)
/// and is used to advertise an absolute POST endpoint to clients.
pub fn router(app_rx: watch::Receiver<Option<AppState>>, base_path: String) -> Router {
    let state = McpState {
        app_rx,
        sessions: Arc::new(Mutex::new(HashMap::new())),
        base_path,
    };
    // Register both the bare and trailing-slash forms of each path. axum 0.7
    // does not auto-redirect "/mcp/sse/" to "/mcp/sse", and many MCP clients
    // (and humans) append a trailing slash, so accept either spelling.
    Router::new()
        .route("/mcp/sse", get(open_sse_stream))
        .route("/mcp/sse/", get(open_sse_stream))
        .route("/mcp/messages", post(receive_message))
        .route("/mcp/messages/", post(receive_message))
        .with_state(state)
}

/// Generates a process-unique session identifier. Not security sensitive: it
/// only needs to be unique among concurrently connected SSE clients.
fn new_session_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = Utc::now().timestamp_nanos_opt().unwrap_or(0);
    format!("{:x}-{:x}", nanos, seq)
}

/// `GET /mcp/sse` — opens the SSE stream for one client session.
async fn open_sse_stream(
    State(state): State<McpState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let session_id = new_session_id();
    let (tx, rx) = mpsc::unbounded_channel::<Event>();

    // Per the HTTP+SSE transport, the very first event tells the client which
    // URL to POST its JSON-RPC messages to (with the session id attached).
    let endpoint = format!(
        "{}/mcp/messages?sessionId={}",
        state.base_path, session_id
    );
    let _ = tx.send(Event::default().event("endpoint").data(endpoint));

    state
        .sessions
        .lock()
        .unwrap()
        .insert(session_id.clone(), tx);
    log::info!("MCP client connected (session {})", session_id);

    let stream = stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|event| (Ok(event), rx))
    });

    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// `POST /mcp/messages?sessionId=...` — receives a single JSON-RPC message,
/// acknowledges with 202, and (for requests) delivers the response over the
/// matching SSE stream.
async fn receive_message(
    State(state): State<McpState>,
    Query(params): Query<HashMap<String, String>>,
    body: String,
) -> Response {
    let Some(session_id) = params.get("sessionId") else {
        return (StatusCode::BAD_REQUEST, "missing sessionId query parameter").into_response();
    };

    let request: Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, format!("invalid JSON: {}", e)).into_response();
        }
    };

    let snapshot = state.app_rx.borrow().clone();
    let response = handle_jsonrpc(&request, snapshot.as_ref());

    if let Some(response) = response {
        let sender = state.sessions.lock().unwrap().get(session_id).cloned();
        match sender {
            Some(sender) => {
                let event = Event::default().event("message").data(response.to_string());
                if sender.send(event).is_err() {
                    // The SSE stream is gone; drop the dead session.
                    state.sessions.lock().unwrap().remove(session_id);
                    return (StatusCode::GONE, "session stream closed").into_response();
                }
            }
            None => {
                return (StatusCode::NOT_FOUND, "unknown sessionId").into_response();
            }
        }
    }

    StatusCode::ACCEPTED.into_response()
}

/// Dispatches a single JSON-RPC 2.0 message. Returns `Some(response)` for
/// requests (those carrying an `id`) and `None` for notifications.
fn handle_jsonrpc(request: &Value, snapshot: Option<&AppState>) -> Option<Value> {
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    let id = request.get("id").cloned();

    // Notifications have no id and expect no response.
    let id = id?;

    let result = match method {
        "initialize" => Ok(json!({
            "protocolVersion": MCP_PROTOCOL_VERSION,
            "capabilities": { "tools": {} },
            "serverInfo": {
                "name": MCP_SERVER_NAME,
                "version": MCP_SERVER_VERSION,
            },
            "instructions":
                "Tools expose a home energy storage controller: live and 24h-historical \
                 solar production, household load, grid flow, battery power and state of \
                 charge, plus the electricity spot-price forecast and solar/heating \
                 forecasts. All powers are in kilowatts (kW), prices in euro-cents per kWh.",
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(tools_list()),
        "tools/call" => handle_tools_call(request, snapshot),
        other => Err((-32601, format!("method not found: {}", other))),
    };

    Some(match result {
        Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
        Err((code, message)) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message },
        }),
    })
}

/// The `tools/list` result: every tool the energy controller exposes.
fn tools_list() -> Value {
    let metric_keys: Vec<&str> = HISTORY_METRICS.iter().map(|m| m.key).collect();

    json!({
        "tools": [
            {
                "name": "get_current_household_energy_status",
                "description":
                    "Latest live reading: battery state of charge (%), solar PV, household load, grid and battery power (kW).",
                "inputSchema": {
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }
            },
            {
                "name": "get_household_energy_history_24h",
                "description":
                    "Rolling 24h history (downsampled to <=48 samples) for one or all energy metrics. Each metric block states its unit.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "metric_name": {
                            "type": "string",
                            "enum": metric_keys,
                            "description": "Metric to return; omit for all."
                        }
                    },
                    "additionalProperties": false
                }
            },
            {
                "name": "get_electricity_spot_price_forecast",
                "description":
                    "Current electricity price, the controller's percentile thresholds, and upcoming hourly spot prices (euro-cents/kWh).",
                "inputSchema": {
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }
            },
            {
                "name": "get_solar_and_heating_power_forecast",
                "description":
                    "Hourly solar PV production and estimated heating demand (kW) for today/tomorrow, plus expected remaining PV yield today (kWh).",
                "inputSchema": {
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }
            },
            {
                "name": "get_battery_and_heatpump_control_decisions",
                "description":
                    "Controller's current target battery SoC, heat-pump compressor allow/block decision, and status message.",
                "inputSchema": {
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }
            }
        ]
    })
}

/// Dispatches `tools/call` to the concrete tool implementation.
fn handle_tools_call(request: &Value, snapshot: Option<&AppState>) -> Result<Value, (i64, String)> {
    let params = request.get("params");
    let name = params
        .and_then(|p| p.get("name"))
        .and_then(Value::as_str)
        .ok_or((-32602, "missing params.name".to_string()))?;
    let arguments = params.and_then(|p| p.get("arguments"));

    let Some(state) = snapshot else {
        return Ok(tool_text(
            "No energy data is available yet. The controller has not published a snapshot \
             since startup; try again in a moment.",
        ));
    };

    match name {
        "get_current_household_energy_status" => Ok(tool_json(current_status_json(state))),
        "get_household_energy_history_24h" => {
            let metric = arguments
                .and_then(|a| a.get("metric_name"))
                .and_then(Value::as_str);
            energy_history_json(state, metric).map(tool_json)
        }
        "get_electricity_spot_price_forecast" => Ok(tool_json(price_forecast_json(state))),
        "get_solar_and_heating_power_forecast" => Ok(tool_json(forecast_json(state))),
        "get_battery_and_heatpump_control_decisions" => Ok(tool_json(decisions_json(state))),
        other => Err((-32602, format!("unknown tool: {}", other))),
    }
}

/// Wraps a plain string as an MCP tool-call result.
fn tool_text(text: &str) -> Value {
    json!({ "content": [ { "type": "text", "text": text } ] })
}

/// Wraps a structured JSON value as an MCP tool-call result (pretty-printed
/// text content, the standard way MCP returns machine-readable payloads).
fn tool_json(value: Value) -> Value {
    let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
    json!({ "content": [ { "type": "text", "text": text } ] })
}

/// Maximum number of points returned for any time series; longer series are
/// downsampled to this many bucket-averaged points to keep responses compact.
const MAX_SERIES_POINTS: usize = 48;

/// Reduces a `(timestamp, value)` series to at most `MAX_SERIES_POINTS` evenly
/// spaced buckets, averaging the timestamp and value within each bucket. Series
/// already at or below the cap are returned unchanged.
fn downsample(points: &[(i64, f64)]) -> Vec<(i64, f64)> {
    let len = points.len();
    if len <= MAX_SERIES_POINTS {
        return points.to_vec();
    }
    (0..MAX_SERIES_POINTS)
        .map(|b| {
            let start = b * len / MAX_SERIES_POINTS;
            let end = ((b + 1) * len / MAX_SERIES_POINTS).max(start + 1).min(len);
            let bucket = &points[start..end];
            let n = bucket.len() as f64;
            let ts = (bucket.iter().map(|p| p.0).sum::<i64>() as f64 / n).round() as i64;
            let value = bucket.iter().map(|p| p.1).sum::<f64>() / n;
            (ts, value)
        })
        .collect()
}

/// Formats a millisecond UNIX timestamp as an ISO-8601 UTC string.
fn iso_from_millis(ts_ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(ts_ms)
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_default()
}

/// Formats a (possibly fractional) UNIX-seconds timestamp as ISO-8601 UTC.
fn iso_from_seconds(ts_s: f64) -> String {
    DateTime::<Utc>::from_timestamp(ts_s as i64, 0)
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_default()
}

fn current_status_json(state: &AppState) -> Value {
    let s = &state.system_status;
    json!({
        "battery_state_of_charge_percent": s.soc,
        "solar_pv_production_kw": s.pv_power,
        "household_consumption_load_kw": s.load_power,
        "grid_power_kw": s.grid_power,
        "battery_power_kw": s.battery_power,
        "field_descriptions": {
            "battery_state_of_charge_percent": "Battery charge level, 0-100%.",
            "solar_pv_production_kw": "Solar PV production power in kW.",
            "household_consumption_load_kw": "Household consumption (load) in kW.",
            "grid_power_kw": "Grid power in kW; positive = importing, negative = exporting.",
            "battery_power_kw": "Battery power in kW; positive = discharging, negative = charging."
        }
    })
}

/// Maps a metric key to its `(timestamp_ms, value)` history series.
fn history_series<'a>(state: &'a AppState, key: &str) -> Option<&'a Vec<(i64, f64)>> {
    let s = &state.system_status;
    match key {
        "battery_state_of_charge_percent" => Some(&s.soc_histogram),
        "solar_pv_production_kw" => Some(&s.pv_histogram),
        "household_consumption_load_kw" => Some(&s.load_histogram),
        "grid_power_kw" => Some(&s.grid_histogram),
        "battery_power_kw" => Some(&s.battuse_histogram),
        _ => None,
    }
}

fn metric_meta(key: &str) -> Option<&'static HistoryMetric> {
    HISTORY_METRICS.iter().find(|m| m.key == key)
}

/// Builds one metric's history block: unit, description and timestamped samples.
fn one_metric_history(state: &AppState, key: &str) -> Value {
    let meta = metric_meta(key).unwrap();
    let samples: Vec<Value> = history_series(state, key)
        .map(|series| {
            downsample(series)
                .iter()
                .map(|(ts_ms, value)| {
                    json!({
                        "timestamp_utc": iso_from_millis(*ts_ms),
                        "timestamp_unix_ms": ts_ms,
                        "value": value,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    json!({
        "metric_name": key,
        "unit": meta.unit,
        "description": meta.description,
        "sample_count": samples.len(),
        "samples": samples,
    })
}

fn energy_history_json(state: &AppState, metric: Option<&str>) -> Result<Value, (i64, String)> {
    match metric {
        Some(key) => {
            if metric_meta(key).is_none() {
                let valid: Vec<&str> = HISTORY_METRICS.iter().map(|m| m.key).collect();
                return Err((
                    -32602,
                    format!("unknown metric_name '{}'; valid values: {}", key, valid.join(", ")),
                ));
            }
            Ok(json!({
                "window": "rolling 24 hours, oldest sample first",
                "metrics": [ one_metric_history(state, key) ],
            }))
        }
        None => {
            let metrics: Vec<Value> = HISTORY_METRICS
                .iter()
                .map(|m| one_metric_history(state, m.key))
                .collect();
            Ok(json!({
                "window": "rolling 24 hours, oldest sample first",
                "metrics": metrics,
            }))
        }
    }
}

fn price_forecast_json(state: &AppState) -> Value {
    let price_points: Vec<(i64, f64)> = state
        .prices
        .iter()
        .map(|p| (p.from.timestamp_millis(), p.price))
        .collect();
    let prices: Vec<Value> = downsample(&price_points)
        .iter()
        .map(|(ts_ms, price)| {
            json!({
                "interval_start_utc": iso_from_millis(*ts_ms),
                "price_euro_cents_per_kwh": price,
            })
        })
        .collect();
    json!({
        "price_unit": "euro-cents per kWh",
        "current_price_euro_cents_per_kwh": state.current_price,
        "heatpump_block_threshold_euro_cents_per_kwh": state.block_threshold,
        "battery_low_price_charge_threshold_euro_cents_per_kwh": state.low_price_threshold,
        "battery_spike_threshold_euro_cents_per_kwh": state.spike_threshold,
        "threshold_descriptions": {
            "heatpump_block_threshold_euro_cents_per_kwh":
                "At or above this price the heat-pump compressor is eligible to be blocked.",
            "battery_low_price_charge_threshold_euro_cents_per_kwh":
                "At or below this price the battery is eligible for force-charging.",
            "battery_spike_threshold_euro_cents_per_kwh":
                "A future price above this is treated as a major spike worth pre-charging for."
        },
        "upcoming_prices": prices,
    })
}

fn forecast_json(state: &AppState) -> Value {
    // Forecast timestamps are whole UNIX seconds stored as f64; downsample on
    // integer seconds, then format back to ISO-8601.
    let pv_points: Vec<(i64, f64)> = state
        .hourly_pv_kw
        .iter()
        .map(|(ts_s, kw)| (*ts_s as i64, *kw))
        .collect();
    let pv: Vec<Value> = downsample(&pv_points)
        .iter()
        .map(|(ts_s, kw)| {
            json!({
                "hour_start_utc": iso_from_seconds(*ts_s as f64),
                "forecast_pv_production_kw": kw,
            })
        })
        .collect();
    let heating_points: Vec<(i64, f64)> = state
        .hourly_heating_kw
        .iter()
        .map(|(ts_s, kw)| (*ts_s as i64, *kw))
        .collect();
    let heating: Vec<Value> = downsample(&heating_points)
        .iter()
        .map(|(ts_s, kw)| {
            json!({
                "hour_start_utc": iso_from_seconds(*ts_s as f64),
                "estimated_heating_demand_kw": kw,
            })
        })
        .collect();
    json!({
        "expected_remaining_pv_yield_today_kwh": state.expected_pv_kwh,
        "hourly_solar_pv_production_forecast_kw": pv,
        "hourly_heating_power_demand_forecast_kw": heating,
    })
}

fn decisions_json(state: &AppState) -> Value {
    json!({
        "target_battery_state_of_charge_percent": state.soc_decision,
        "heatpump_compressor_decision": format!("{:?}", state.compressor_decision),
        "heatpump_compressor_decision_meaning":
            "'Allowed' = compressor may run; 'Blocked' = compressor is being inhibited \
             because the current price is in an expensive percentile.",
        "controller_status_message": state.status_message,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PriceInfo, SystemStatus};
    use std::io::{Read, Write};
    use std::net::TcpStream;

    /// Builds an AppState populated with recognisable values for assertions.
    fn sample_state() -> AppState {
        let mut status = SystemStatus::default();
        status.soc = 73;
        status.pv_power = 4.2;
        status.load_power = 1.1;
        status.grid_power = -3.0; // exporting
        status.battery_power = 0.9; // discharging
        status.soc_histogram = vec![(1_700_000_000_000, 70.0), (1_700_003_600_000, 73.0)];
        status.pv_histogram = vec![(1_700_000_000_000, 3.5)];

        let mut state = AppState::new_with_status("running normally".to_string());
        state.system_status = status;
        state.current_price = 18.5;
        state.block_threshold = 30.0;
        state.low_price_threshold = 8.0;
        state.spike_threshold = 45.0;
        state.soc_decision = 50;
        state.prices = vec![PriceInfo {
            from: DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap(),
            price: 18.5,
        }];
        state.expected_pv_kwh = 22.4;
        state.hourly_pv_kw = vec![(1_700_000_000.0, 3.5)];
        state.hourly_heating_kw = vec![(1_700_000_000.0, 0.8)];
        state
    }

    fn call(name: &str, args: Value, state: &AppState) -> Value {
        let req = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": name, "arguments": args },
        });
        let resp = handle_jsonrpc(&req, Some(state)).expect("request expects a response");
        // Pull the text payload back out and re-parse it as structured JSON.
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn initialize_reports_server_identity_and_tool_capability() {
        let req = json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{}});
        let resp = handle_jsonrpc(&req, None).unwrap();
        assert_eq!(resp["result"]["serverInfo"]["name"], MCP_SERVER_NAME);
        assert_eq!(resp["result"]["protocolVersion"], MCP_PROTOCOL_VERSION);
        assert!(resp["result"]["capabilities"]["tools"].is_object());
    }

    #[test]
    fn notifications_produce_no_response() {
        let req = json!({"jsonrpc":"2.0","method":"notifications/initialized"});
        assert!(handle_jsonrpc(&req, None).is_none());
    }

    #[test]
    fn tools_list_exposes_all_five_tools() {
        let req = json!({"jsonrpc":"2.0","id":1,"method":"tools/list"});
        let resp = handle_jsonrpc(&req, None).unwrap();
        let tools = resp["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"get_current_household_energy_status"));
        assert!(names.contains(&"get_household_energy_history_24h"));
        assert!(names.contains(&"get_electricity_spot_price_forecast"));
        assert!(names.contains(&"get_solar_and_heating_power_forecast"));
        assert!(names.contains(&"get_battery_and_heatpump_control_decisions"));
    }

    #[test]
    fn current_status_carries_units_and_sign_conventions() {
        let state = sample_state();
        let out = call("get_current_household_energy_status", json!({}), &state);
        assert_eq!(out["battery_state_of_charge_percent"], 73);
        assert_eq!(out["solar_pv_production_kw"], 4.2);
        assert_eq!(out["grid_power_kw"], -3.0);
        assert!(out["field_descriptions"]["grid_power_kw"]
            .as_str()
            .unwrap()
            .contains("importing"));
    }

    #[test]
    fn history_single_metric_returns_iso_timestamped_samples() {
        let state = sample_state();
        let out = call(
            "get_household_energy_history_24h",
            json!({ "metric_name": "battery_state_of_charge_percent" }),
            &state,
        );
        let metrics = out["metrics"].as_array().unwrap();
        assert_eq!(metrics.len(), 1);
        assert_eq!(metrics[0]["metric_name"], "battery_state_of_charge_percent");
        assert_eq!(metrics[0]["sample_count"], 2);
        let first = &metrics[0]["samples"][0];
        assert_eq!(first["value"], 70.0);
        assert!(first["timestamp_utc"].as_str().unwrap().starts_with("2023-"));
    }

    #[test]
    fn long_history_is_downsampled_to_48_points() {
        let mut state = sample_state();
        // 288 raw samples (24h at 5-min resolution) must collapse to 48.
        state.system_status.soc_histogram = (0..288)
            .map(|i| (1_700_000_000_000 + i as i64 * 300_000, 50.0 + (i % 10) as f64))
            .collect();
        let out = call(
            "get_household_energy_history_24h",
            json!({ "metric_name": "battery_state_of_charge_percent" }),
            &state,
        );
        let metric = &out["metrics"][0];
        assert_eq!(metric["sample_count"], 48);
        assert_eq!(metric["samples"].as_array().unwrap().len(), 48);
    }

    #[test]
    fn short_series_is_returned_unchanged() {
        assert_eq!(downsample(&[(0, 1.0), (1, 2.0)]), vec![(0, 1.0), (1, 2.0)]);
    }

    #[test]
    fn history_without_metric_returns_every_channel() {
        let state = sample_state();
        let out = call("get_household_energy_history_24h", json!({}), &state);
        assert_eq!(out["metrics"].as_array().unwrap().len(), HISTORY_METRICS.len());
    }

    #[test]
    fn history_rejects_unknown_metric() {
        let state = sample_state();
        let req = json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params": {"name":"get_household_energy_history_24h","arguments":{"metric_name":"bogus"}},
        });
        let resp = handle_jsonrpc(&req, Some(&state)).unwrap();
        assert_eq!(resp["error"]["code"], -32602);
    }

    #[test]
    fn price_forecast_reports_thresholds_and_prices() {
        let state = sample_state();
        let out = call("get_electricity_spot_price_forecast", json!({}), &state);
        assert_eq!(out["current_price_euro_cents_per_kwh"], 18.5);
        assert_eq!(out["heatpump_block_threshold_euro_cents_per_kwh"], 30.0);
        assert_eq!(out["upcoming_prices"].as_array().unwrap().len(), 1);
        assert_eq!(out["upcoming_prices"][0]["price_euro_cents_per_kwh"], 18.5);
    }

    #[test]
    fn tools_call_without_snapshot_reports_no_data_yet() {
        let req = json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params": {"name":"get_current_household_energy_status","arguments":{}},
        });
        let resp = handle_jsonrpc(&req, None).unwrap();
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("No energy data is available yet"));
    }

    /// End-to-end check of the actual HTTP+SSE transport: opens the SSE stream,
    /// reads the advertised endpoint, POSTs an `initialize` request and confirms
    /// the JSON-RPC response arrives back as an SSE `message` event. Parameterised
    /// over the SSE path so we cover both the bare and trailing-slash spellings.
    async fn run_round_trip(sse_path: &'static str) -> String {
        let (tx, rx) = watch::channel(Some(sample_state()));
        let _keep = tx; // keep the sender alive for the duration of the test

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = router(rx, String::new());
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        // Open the SSE stream on a blocking socket so we can read it line by line.
        tokio::task::spawn_blocking(move || {
            let mut stream = TcpStream::connect(addr).unwrap();
            stream
                .write_all(
                    format!(
                        "GET {} HTTP/1.1\r\nHost: {}\r\nAccept: text/event-stream\r\n\r\n",
                        sse_path, addr
                    )
                    .as_bytes(),
                )
                .unwrap();
            read_endpoint_and_then_post(stream, addr)
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn sse_transport_round_trip() {
        let sse = run_round_trip("/mcp/sse").await;
        assert!(sse.contains("\"protocolVersion\":\"2024-11-05\""), "got: {}", sse);
        assert!(sse.contains(MCP_SERVER_NAME));
    }

    #[tokio::test]
    async fn sse_transport_accepts_trailing_slash() {
        let sse = run_round_trip("/mcp/sse/").await;
        assert!(sse.contains("\"protocolVersion\":\"2024-11-05\""), "got: {}", sse);
    }

    /// Reads the `endpoint` SSE event, POSTs an initialize request to it on a
    /// second connection, then reads the SSE stream until the `message` event
    /// carrying the JSON-RPC response arrives. Returns that response text.
    fn read_endpoint_and_then_post(mut sse: TcpStream, addr: std::net::SocketAddr) -> String {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 1024];
        let mut endpoint: Option<String> = None;

        loop {
            let n = sse.read(&mut tmp).unwrap();
            if n == 0 {
                panic!("SSE stream closed before endpoint event");
            }
            buf.extend_from_slice(&tmp[..n]);
            let text = String::from_utf8_lossy(&buf);

            if endpoint.is_none() {
                if let Some(idx) = text.find("event: endpoint") {
                    if let Some(data_idx) = text[idx..].find("data: ") {
                        let rest = &text[idx + data_idx + 6..];
                        if let Some(eol) = rest.find('\n') {
                            endpoint = Some(rest[..eol].trim().to_string());
                            // Fire the POST request once we know where to send it.
                            let path = endpoint.clone().unwrap();
                            let body = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
                            let mut post = TcpStream::connect(addr).unwrap();
                            post.write_all(
                                format!(
                                    "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                                    path, addr, body.len(), body
                                )
                                .as_bytes(),
                            )
                            .unwrap();
                            let mut _resp = String::new();
                            post.read_to_string(&mut _resp).unwrap();
                        }
                    }
                }
            }

            // Once the endpoint is known, look for the JSON-RPC response event.
            if endpoint.is_some() && text.contains("\"result\"") {
                return text.into_owned();
            }
        }
    }
}
