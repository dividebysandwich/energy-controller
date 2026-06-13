use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use reqwest::blocking::Client;
use reqwest::header::{CONTENT_TYPE, COOKIE};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::VecDeque;

use crate::SystemStatus;

const HISTORY_WINDOW_MS: i64 = 24 * 60 * 60 * 1000;

#[derive(Clone, Debug)]
struct HistorySample {
    ts_ms: i64,
    soc: f64,
    pv_kw: f64,
    load_kw: f64,
    grid_kw: f64,
    battery_kw: f64,
}

#[derive(Deserialize, Debug, Clone)]
#[allow(dead_code)]
struct StatusDataPoint {
    pub time: i64,
    #[serde(rename = "BatterySOC")]
    pub battery_soc: f64,
    #[serde(rename = "PV")]
    pub pv: f64,
    #[serde(rename = "Consumption")]
    pub consumption: f64,
    #[serde(rename = "Grid")]
    pub grid: f64,
    #[serde(rename = "BatteryPower")]
    pub battery_power: f64,
}

pub struct LegacySource {
    pub url: String,
}

pub struct HuaweiSource {
    pub api_url: String,
    pub username: String,
    pub system_code: String,
    pub station_code: Option<String>,
    pub invert_grid_sign: bool,
    session: Option<HuaweiSession>,
    devices: Option<HuaweiDevices>,
}

impl HuaweiSource {
    pub fn new(
        api_url: String,
        username: String,
        system_code: String,
        station_code: Option<String>,
        invert_grid_sign: bool,
    ) -> Self {
        Self {
            api_url,
            username,
            system_code,
            station_code,
            invert_grid_sign,
            session: None,
            devices: None,
        }
    }
}

struct HuaweiSession {
    xsrf_token: String,
    cookie_header: String,
    logged_in_at: DateTime<Utc>,
}

#[derive(Default, Clone)]
struct HuaweiDevices {
    inverter_ids: Vec<String>,
    battery_ids: Vec<String>,
    meter_ids: Vec<String>,
}

pub struct SolarEdgeSource {
    pub api_url: String,
    pub api_key: String,
    pub site_id: String,
}

/// A fast "live" status source: a URL returning a JSON object with the current
/// scalar readings (`soc`, `pv`, `consumption`, `grid`, `batteryuse`), updated
/// every few seconds. It provides ONLY the live current values — historical
/// charts continue to come from the other configured sources. Powers are
/// expected in kW and SOC in percent.
pub struct LiveSource {
    pub url: String,
}

#[derive(Deserialize, Debug)]
struct LiveStatusPayload {
    soc: f64,
    pv: f64,
    consumption: f64,
    grid: f64,
    batteryuse: f64,
}

pub struct DataSources {
    pub legacy: Option<LegacySource>,
    pub huawei: Option<HuaweiSource>,
    pub solaredge: Option<SolarEdgeSource>,
    pub live: Option<LiveSource>,
    history: VecDeque<HistorySample>,
}

impl DataSources {
    pub fn new(
        legacy: Option<LegacySource>,
        huawei: Option<HuaweiSource>,
        solaredge: Option<SolarEdgeSource>,
        live: Option<LiveSource>,
    ) -> Self {
        Self {
            legacy,
            huawei,
            solaredge,
            live,
            history: VecDeque::new(),
        }
    }

    pub fn fetch_status(&mut self, client: &Client) -> Result<SystemStatus> {
        let mut status = SystemStatus::default();
        let mut have_legacy = false;
        let mut have_huawei = false;
        let mut have_solaredge = false;
        let mut errors: Vec<String> = Vec::new();

        if let Some(legacy) = &self.legacy {
            match legacy.fetch(client) {
                Ok(s) => {
                    status = s;
                    have_legacy = true;
                }
                Err(e) => errors.push(format!("legacy: {}", e)),
            }
        }

        if let Some(huawei) = &mut self.huawei {
            match huawei.fetch(client) {
                Ok(s) => {
                    status.soc = s.soc;
                    status.load_power = s.load_power;
                    status.battery_power = s.battery_power;
                    if have_legacy || have_solaredge {
                        status.pv_power += s.pv_power;
                        status.grid_power += s.grid_power;
                    } else {
                        status.pv_power = s.pv_power;
                        status.grid_power = s.grid_power;
                    }
                    have_huawei = true;
                }
                Err(e) => errors.push(format!("huawei: {}", e)),
            }
        }

        if let Some(se) = &self.solaredge {
            match se.fetch(client) {
                Ok(s) => {
                    if have_legacy || have_huawei {
                        status.pv_power += s.pv_power;
                        status.grid_power += s.grid_power;
                    } else {
                        status.pv_power = s.pv_power;
                        status.grid_power = s.grid_power;
                    }
                    have_solaredge = true;
                }
                Err(e) => errors.push(format!("solaredge: {}", e)),
            }
        }

        // For non-legacy sources we accumulate a 24h rolling history in-process,
        // since neither the FusionSolar nor the SolarEdge endpoints we use return
        // power-vs-time history at the resolution this chart needs. When legacy is
        // combined with another source the live values are summed across systems,
        // so legacy's server-side histograms no longer match the displayed values
        // and we replace them with the internal buffer too. This runs BEFORE the
        // optional live override so the historical charts keep coming from these
        // sources, not the live endpoint.
        let any_non_legacy = have_huawei || have_solaredge;
        if any_non_legacy {
            let now_ms = Utc::now().timestamp_millis();
            self.history.push_back(HistorySample {
                ts_ms: now_ms,
                soc: status.soc as f64,
                pv_kw: status.pv_power,
                load_kw: status.load_power,
                grid_kw: status.grid_power,
                battery_kw: status.battery_power,
            });
            let cutoff = now_ms - HISTORY_WINDOW_MS;
            while let Some(front) = self.history.front() {
                if front.ts_ms < cutoff {
                    self.history.pop_front();
                } else {
                    break;
                }
            }
            status.soc_histogram =
                self.history.iter().map(|s| (s.ts_ms, s.soc)).collect();
            status.pv_histogram =
                self.history.iter().map(|s| (s.ts_ms, s.pv_kw)).collect();
            status.load_histogram =
                self.history.iter().map(|s| (s.ts_ms, s.load_kw)).collect();
            status.grid_histogram =
                self.history.iter().map(|s| (s.ts_ms, s.grid_kw)).collect();
            status.battuse_histogram =
                self.history.iter().map(|s| (s.ts_ms, s.battery_kw)).collect();
        }

        // Optional fast live source: overrides ONLY the current scalar readings
        // (SOC, PV, load, grid, battery power). The histograms built above are
        // left untouched, so historical charts keep their original data source.
        let mut have_live = false;
        if let Some(live) = &self.live {
            match live.fetch(client) {
                Ok(s) => {
                    status.soc = s.soc;
                    status.pv_power = s.pv_power;
                    status.load_power = s.load_power;
                    status.grid_power = s.grid_power;
                    status.battery_power = s.battery_power;
                    have_live = true;
                }
                Err(e) => errors.push(format!("live: {}", e)),
            }
        }

        if !have_legacy && !have_huawei && !have_solaredge && !have_live {
            return Err(anyhow!(
                "all configured data sources failed: {}",
                errors.join("; ")
            ));
        }
        if !errors.is_empty() {
            log::warn!("Some data sources failed: {}", errors.join("; "));
        }

        Ok(status)
    }
}

impl LegacySource {
    fn fetch(&self, client: &Client) -> Result<SystemStatus> {
        let response = client
            .get(&self.url)
            .send()
            .with_context(|| format!("Failed to connect to status URL: {}", self.url))?;
        if !response.status().is_success() {
            return Err(anyhow!("Status URL returned {}", response.status()));
        }
        let payload: Vec<StatusDataPoint> =
            response.json().context("Failed to parse status JSON")?;
        if payload.is_empty() {
            return Err(anyhow!("Status URL returned an empty array"));
        }
        let current = payload.last().unwrap();

        let soc_histogram: Vec<(i64, f64)> =
            payload.iter().map(|p| (p.time, p.battery_soc)).collect();
        let pv_histogram: Vec<(i64, f64)> =
            payload.iter().map(|p| (p.time, p.pv / 1000.0)).collect();
        let consumption_histogram: Vec<(i64, f64)> = payload
            .iter()
            .map(|p| (p.time, p.consumption / 1000.0))
            .collect();
        let grid_histogram: Vec<(i64, f64)> =
            payload.iter().map(|p| (p.time, p.grid / 1000.0)).collect();
        let battuse_histogram: Vec<(i64, f64)> = payload
            .iter()
            .map(|p| (p.time, p.battery_power / 1000.0))
            .collect();

        Ok(SystemStatus {
            soc: current.battery_soc as u8,
            pv_power: current.pv / 1000.0,
            load_power: current.consumption / 1000.0,
            grid_power: current.grid / 1000.0,
            battery_power: current.battery_power / 1000.0,
            soc_histogram,
            pv_histogram,
            load_histogram: consumption_histogram,
            grid_histogram,
            battuse_histogram,
        })
    }
}

impl HuaweiSource {
    fn ensure_session(&mut self, client: &Client) -> Result<()> {
        let needs_login = match &self.session {
            None => true,
            // FusionSolar tokens expire after 30 minutes of inactivity.
            Some(s) => Utc::now().signed_duration_since(s.logged_in_at).num_minutes() > 25,
        };
        if !needs_login {
            return Ok(());
        }

        let url = format!(
            "{}/thirdData/login",
            self.api_url.trim_end_matches('/')
        );
        let body = json!({
            "userName": self.username,
            "systemCode": self.system_code,
        });
        let resp = client
            .post(&url)
            .header(CONTENT_TYPE, "application/json")
            .json(&body)
            .send()
            .context("FusionSolar login request failed")?;

        let status_code = resp.status();
        let mut xsrf: Option<String> = None;
        let mut cookies: Vec<String> = Vec::new();
        for hv in resp.headers().get_all("set-cookie").iter() {
            if let Ok(s) = hv.to_str() {
                let cookie = s.split(';').next().unwrap_or("").trim().to_string();
                if let Some((name, value)) = cookie.split_once('=') {
                    if name.eq_ignore_ascii_case("XSRF-TOKEN") {
                        xsrf = Some(value.to_string());
                    }
                    cookies.push(format!("{}={}", name, value));
                }
            }
        }
        let body_text = resp.text().unwrap_or_default();

        if !status_code.is_success() {
            return Err(anyhow!(
                "FusionSolar login HTTP {}: {}",
                status_code,
                body_text
            ));
        }

        if let Ok(body_json) = serde_json::from_str::<Value>(&body_text) {
            if body_json.get("success") != Some(&Value::Bool(true)) {
                return Err(anyhow!("FusionSolar login failed: {}", body_text));
            }
        }

        let xsrf = xsrf.context("FusionSolar login: no XSRF-TOKEN cookie returned")?;

        self.session = Some(HuaweiSession {
            xsrf_token: xsrf,
            cookie_header: cookies.join("; "),
            logged_in_at: Utc::now(),
        });
        Ok(())
    }

    fn post_json(&mut self, client: &Client, path: &str, body: &Value) -> Result<Value> {
        self.ensure_session(client)?;
        let session = self.session.as_ref().unwrap();
        let url = format!("{}{}", self.api_url.trim_end_matches('/'), path);
        let resp = client
            .post(&url)
            .header(CONTENT_TYPE, "application/json")
            .header("XSRF-TOKEN", &session.xsrf_token)
            .header(COOKIE, &session.cookie_header)
            .json(body)
            .send()
            .with_context(|| format!("FusionSolar POST {} failed", path))?;
        if !resp.status().is_success() {
            return Err(anyhow!("FusionSolar POST {} HTTP {}", path, resp.status()));
        }
        let json: Value = resp
            .json()
            .with_context(|| format!("FusionSolar parse {} response", path))?;
        if let Some(fc) = json.get("failCode").and_then(|v| v.as_i64()) {
            // 305/401 indicates token expired; force re-login on next call.
            if fc == 305 || fc == 401 {
                self.session = None;
                return Err(anyhow!("FusionSolar token expired (failCode {})", fc));
            }
        }
        if json.get("success") != Some(&Value::Bool(true)) {
            return Err(anyhow!("FusionSolar API error on {}: {}", path, json));
        }
        Ok(json)
    }

    fn ensure_station_and_devices(&mut self, client: &Client) -> Result<()> {
        if self.devices.is_some() && self.station_code.is_some() {
            return Ok(());
        }

        if self.station_code.is_none() {
            // Try newer "getPlantList" first, fall back to legacy "getStationList".
            let resp = self
                .post_json(client, "/thirdData/stations", &json!({"pageNo": 1}))
                .or_else(|_| self.post_json(client, "/thirdData/getStationList", &json!({})))?;
            let list = resp
                .get("data")
                .and_then(|d| d.get("list").or(Some(d)))
                .and_then(|v| v.as_array())
                .cloned()
                .or_else(|| resp.get("data").and_then(|d| d.as_array()).cloned())
                .context("getStationList: no data array")?;
            let first = list.first().context("getStationList: empty list")?;
            let code = first
                .get("plantCode")
                .or_else(|| first.get("stationCode"))
                .and_then(|v| v.as_str())
                .context("getStationList: no station/plant code field")?;
            self.station_code = Some(code.to_string());
        }

        let station = self.station_code.clone().unwrap();
        let resp = self.post_json(
            client,
            "/thirdData/getDevList",
            &json!({"stationCodes": station}),
        )?;
        let data = resp
            .get("data")
            .and_then(|d| d.as_array())
            .context("getDevList: no data")?;
        let mut devices = HuaweiDevices::default();
        for d in data {
            let dev_id = d
                .get("id")
                .or_else(|| d.get("devId"))
                .or_else(|| d.get("devDn"));
            let dev_id_str = match dev_id {
                Some(Value::Number(n)) => Some(n.to_string()),
                Some(Value::String(s)) => Some(s.clone()),
                _ => None,
            };
            let dev_type = d.get("devTypeId").and_then(|v| v.as_i64());
            if let (Some(id), Some(t)) = (dev_id_str, dev_type) {
                match t {
                    // 1 = string inverter, 38 = residential inverter
                    1 | 38 => devices.inverter_ids.push(id),
                    // 39 = battery (LUNA)
                    39 => devices.battery_ids.push(id),
                    // 47 = power sensor (smart meter)
                    47 => devices.meter_ids.push(id),
                    _ => {}
                }
            }
        }
        log::debug!(
            "FusionSolar devices discovered: inverters={:?}, batteries={:?}, meters={:?}",
            devices.inverter_ids,
            devices.battery_ids,
            devices.meter_ids
        );
        self.devices = Some(devices);
        Ok(())
    }

    fn fetch_dev_real_kpi(
        &mut self,
        client: &Client,
        dev_ids: &[String],
        dev_type_id: i64,
    ) -> Result<Value> {
        let ids = dev_ids.join(",");
        self.post_json(
            client,
            "/thirdData/getDevRealKpi",
            &json!({
                "devIds": ids,
                "devTypeId": dev_type_id,
            }),
        )
    }

    fn fetch(&mut self, client: &Client) -> Result<SystemStatus> {
        self.ensure_station_and_devices(client)?;
        let devices = self.devices.clone().unwrap();

        let mut pv_power_kw = 0.0;
        let mut grid_power_kw = 0.0;
        let mut battery_power_kw = 0.0;
        let mut soc_pct: u8 = 0;
        let mut got_inverter = false;
        let mut got_battery = false;
        let mut got_meter = false;

        if !devices.inverter_ids.is_empty() {
            // Try residential (38) first; if no rows come back, retry as string inverter (1).
            for type_id in [38i64, 1] {
                let resp = self.fetch_dev_real_kpi(client, &devices.inverter_ids, type_id);
                if let Ok(resp) = resp {
                    if let Some(data) = resp.get("data").and_then(|d| d.as_array()) {
                        let mut any_rows = false;
                        for entry in data {
                            if let Some(map) = entry.get("dataItemMap") {
                                if let Some(p) =
                                    map.get("active_power").and_then(|v| v.as_f64())
                                {
                                    pv_power_kw += p;
                                    any_rows = true;
                                }
                            }
                        }
                        if any_rows {
                            got_inverter = true;
                            break;
                        }
                    }
                }
            }
        }

        if !devices.battery_ids.is_empty() {
            if let Ok(resp) = self.fetch_dev_real_kpi(client, &devices.battery_ids, 39) {
                if let Some(data) = resp.get("data").and_then(|d| d.as_array()) {
                    for entry in data {
                        if let Some(map) = entry.get("dataItemMap") {
                            if let Some(s) =
                                map.get("battery_soc").and_then(|v| v.as_f64())
                            {
                                soc_pct = s.round().clamp(0.0, 100.0) as u8;
                                got_battery = true;
                            }
                            // ch_discharge_power: positive = discharge, negative = charge.
                            if let Some(p) = map
                                .get("ch_discharge_power")
                                .and_then(|v| v.as_f64())
                            {
                                battery_power_kw += p;
                            }
                        }
                    }
                }
            }
        }

        if !devices.meter_ids.is_empty() {
            if let Ok(resp) = self.fetch_dev_real_kpi(client, &devices.meter_ids, 47) {
                if let Some(data) = resp.get("data").and_then(|d| d.as_array()) {
                    for entry in data {
                        if let Some(map) = entry.get("dataItemMap") {
                            if let Some(p) =
                                map.get("active_power").and_then(|v| v.as_f64())
                            {
                                grid_power_kw += p;
                                got_meter = true;
                            }
                        }
                    }
                }
            }
        }

        if !got_inverter && !got_battery && !got_meter {
            return Err(anyhow!(
                "FusionSolar: no usable data from any device (check station/device IDs)"
            ));
        }

        if self.invert_grid_sign {
            grid_power_kw = -grid_power_kw;
        }

        // Approximate household consumption from energy balance:
        //   load = PV + battery_discharge + grid_import
        // (signs assume: pv >= 0, battery_power > 0 when discharging,
        //  grid_power > 0 when importing from grid)
        let mut load_power_kw = pv_power_kw + battery_power_kw + grid_power_kw;
        if load_power_kw < 0.0 {
            load_power_kw = 0.0;
        }

        Ok(SystemStatus {
            soc: soc_pct,
            pv_power: pv_power_kw,
            load_power: load_power_kw,
            grid_power: grid_power_kw,
            battery_power: battery_power_kw,
            soc_histogram: vec![],
            pv_histogram: vec![],
            load_histogram: vec![],
            grid_histogram: vec![],
            battuse_histogram: vec![],
        })
    }
}

impl SolarEdgeSource {
    fn fetch(&self, client: &Client) -> Result<SystemStatus> {
        let url = format!(
            "{}/site/{}/currentPowerFlow?api_key={}",
            self.api_url.trim_end_matches('/'),
            self.site_id,
            self.api_key
        );
        let resp = client
            .get(&url)
            .send()
            .context("SolarEdge currentPowerFlow request failed")?;
        if !resp.status().is_success() {
            return Err(anyhow!("SolarEdge HTTP {}", resp.status()));
        }
        let json: Value = resp.json().context("SolarEdge JSON parse failed")?;
        let flow = json
            .get("siteCurrentPowerFlow")
            .context("SolarEdge response missing siteCurrentPowerFlow")?;

        let unit = flow
            .get("unit")
            .and_then(|v| v.as_str())
            .unwrap_or("kW");
        let to_kw = |v: f64| {
            if unit.eq_ignore_ascii_case("W") {
                v / 1000.0
            } else if unit.eq_ignore_ascii_case("MW") {
                v * 1000.0
            } else {
                v
            }
        };

        let pv_power = flow
            .get("PV")
            .and_then(|p| p.get("currentPower"))
            .and_then(|v| v.as_f64())
            .map(to_kw)
            .unwrap_or(0.0);

        let grid_magnitude = flow
            .get("GRID")
            .and_then(|p| p.get("currentPower"))
            .and_then(|v| v.as_f64())
            .map(to_kw)
            .unwrap_or(0.0);

        // Convention used elsewhere in this program: grid_power > 0 means importing
        // from the grid, < 0 means exporting. SolarEdge reports magnitude only;
        // direction is conveyed via the connections array.
        let mut signed_grid = grid_magnitude;
        if let Some(conns) = flow.get("connections").and_then(|v| v.as_array()) {
            let mut importing = false;
            let mut exporting = false;
            for c in conns {
                let from = c.get("from").and_then(|v| v.as_str()).unwrap_or("");
                let to = c.get("to").and_then(|v| v.as_str()).unwrap_or("");
                if from.eq_ignore_ascii_case("GRID") {
                    importing = true;
                }
                if to.eq_ignore_ascii_case("GRID") {
                    exporting = true;
                }
            }
            if exporting && !importing {
                signed_grid = -grid_magnitude;
            }
        }

        Ok(SystemStatus {
            soc: 0,
            pv_power,
            load_power: 0.0,
            grid_power: signed_grid,
            battery_power: 0.0,
            soc_histogram: vec![],
            pv_histogram: vec![],
            load_histogram: vec![],
            grid_histogram: vec![],
            battuse_histogram: vec![],
        })
    }
}

impl LiveSource {
    fn fetch(&self, client: &Client) -> Result<SystemStatus> {
        let response = client
            .get(&self.url)
            .send()
            .with_context(|| format!("Failed to connect to live status URL: {}", self.url))?;
        if !response.status().is_success() {
            return Err(anyhow!("Live status URL returned {}", response.status()));
        }
        let p: LiveStatusPayload = response
            .json()
            .context("Failed to parse live status JSON")?;

        // Only the scalar current readings are taken from this source; the
        // histogram fields stay empty so the caller leaves the existing
        // historical data untouched. Values are already in kW (SOC in percent).
        // This source reports battery CHARGING as a positive `batteryuse`, which
        // matches the rest of the program (positive = charging), so it is passed
        // through unchanged.
        Ok(SystemStatus {
            soc: p.soc.round().clamp(0.0, 100.0) as u8,
            pv_power: p.pv,
            load_power: p.consumption,
            grid_power: p.grid,
            battery_power: p.batteryuse,
            soc_histogram: vec![],
            pv_histogram: vec![],
            load_histogram: vec![],
            grid_histogram: vec![],
            battuse_histogram: vec![],
        })
    }
}
