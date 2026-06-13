use anyhow::{Context, Result};
use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, Timelike, Utc};
use chrono_tz::Tz;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use ssh2::Session;
use std::{
    cmp,
    collections::VecDeque,
    env,
    io::Read,
    net::TcpStream,
    sync::{Arc, Mutex},
    thread, time,
};

mod datasource;
mod mcp;
mod web;

use datasource::{DataSources, HuaweiSource, LegacySource, LiveSource, SolarEdgeSource};

/// Represents a single price point from the API.
#[derive(Deserialize, Serialize, Debug, Clone)]
struct PriceInfo {
    from: DateTime<Utc>,
    price: f64,
}

/// Represents the live status of the house/battery system.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SystemStatus {
    pub soc: u8,
    pub pv_power: f64,
    pub load_power: f64,
    pub grid_power: f64,
    pub battery_power: f64,
    pub soc_histogram: Vec<(i64, f64)>,
    pub pv_histogram: Vec<(i64, f64)>,
    pub load_histogram: Vec<(i64, f64)>,
    pub grid_histogram: Vec<(i64, f64)>,
    pub battuse_histogram: Vec<(i64, f64)>,
}

/// Represents the desired state of the heatpump compressor.
#[derive(Debug, PartialEq, Eq, Clone, Copy, Serialize, Deserialize)]
enum CompressorState {
    Allowed,
    Blocked,
}

#[derive(Debug, Clone)]
enum BatteryDecision {
    Baseline(u8),
    ForceCharge(u8, String), // SOC, Reason
    WaitForLower(u8, f64),   // Baseline SOC for now, future lower price
}

#[derive(Deserialize, Debug, Clone, Default)]
struct HourlyForecast {
    time: Vec<String>,
    temperature_2m: Vec<f64>,
    shortwave_radiation: Vec<f64>,
}

#[derive(Deserialize, Debug, Clone, Default)]
struct WeatherResponse {
    hourly: HourlyForecast,
}

struct WeatherCache {
    forecast: WeatherResponse,
    last_fetched: DateTime<Utc>,
}

/// Holds all configuration loaded from environment variables.
struct Config {
    // General
    check_interval: time::Duration,
    status_poll_interval: time::Duration,
    enable_pricing: bool,
    // Data source selection
    use_legacy_status: bool,
    use_huawei: bool,
    use_solaredge: bool,
    use_live_status: bool,
    // Legacy status URL
    status_url: String,
    // Fast live status source (current scalar values only; histograms unchanged)
    live_status_url: String,
    // Huawei FusionSolar
    huawei_api_url: String,
    huawei_username: String,
    huawei_system_code: String,
    huawei_station_code: Option<String>,
    huawei_invert_grid_sign: bool,
    // SolarEdge
    solaredge_api_url: String,
    solaredge_api_key: String,
    solaredge_site_id: String,
    // Heatpump / relay settings
    enable_heatpump_control: bool,
    shelly_ip: String,
    relay_on_to_block: bool,
    // Relative Heatpump logic
    block_price_percentile: f64,
    max_continuous_block_minutes: i64,
    min_rest_time_minutes: i64,
    // Relative Battery logic
    lookahead_hours: i64,
    low_price_percentile: f64,
    high_spike_percentile: f64,
    min_spike_difference_cents: f64,
    // Battery control settings
    enable_battery_control: bool,
    ssh_host: String,
    ssh_user: String,
    ssh_pass: String,
    winter_force_charge_soc: u8,
    summer_force_charge_soc: u8,
    summer_min_soc: u8,
    winter_min_soc: u8,
    lat: f64,
    lon: f64,
    battery_size_kwh: f64,
    pv_size_kwp: f64,
    base_load_kw: f64,
    heating_off_temp_c: f64,
    heating_kwh_per_h_at_0c: f64,
}

/// Holds all the data needed for the UI to render a frame.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct AppState {
    prices: Vec<PriceInfo>,
    current_price: f64,
    block_threshold: f64,
    low_price_threshold: f64,
    spike_threshold: f64,
    compressor_decision: CompressorState,
    soc_decision: u8,
    system_status: SystemStatus,
    status_message: String,
    expected_pv_kwh: f64,
    hourly_pv_kw: Vec<(f64, f64)>,
    hourly_heating_kw: Vec<(f64, f64)>,
    show_thresholds: bool,
    show_decisions: bool,
}

fn main() -> Result<()> {
    dotenv::dotenv().ok();
    // Log to stderr. Defaults to `info`; override with the RUST_LOG env var
    // (e.g. RUST_LOG=debug) for more detail.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_secs()
        .init();
    let config = Arc::new(load_config()?);

    // Using watch channel explicitly from tokio
    let (tx, rx) = tokio::sync::watch::channel(None::<AppState>);

    // Spawn Web Server Thread
    let rx_for_web = rx.clone();
    thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            web::start_server(rx_for_web).await;
        });
    });

    // Latest live system status, refreshed by the fast status loop and consumed
    // by the slower control loop. `None` until the first successful fetch.
    let latest_status: Arc<Mutex<Option<SystemStatus>>> = Arc::new(Mutex::new(None));

    // Fast status-polling loop: owns the data sources and refreshes the live
    // system status every `status_poll_interval`, independent of the (slower)
    // price/decision control loop, so the web UI stays close to real time.
    spawn_status_loop(config.clone(), tx.clone(), rx.clone(), latest_status.clone());

    let config_ctrl = config.clone();
    let latest_status_ctrl = latest_status.clone();
    let controller = thread::spawn(move || {
        let config = config_ctrl;
        let latest_status = latest_status_ctrl;
        let client = Client::builder().use_rustls_tls().build().unwrap();
        let mut prices: VecDeque<PriceInfo> = VecDeque::new();
        let mut last_compressor_state = CompressorState::Allowed;
        let mut last_soc_set: u8 = 0;
        let mut block_start_time: Option<DateTime<Utc>> = None;
        let mut rest_until_time: Option<DateTime<Utc>> = None;
        let mut weather_cache: Option<WeatherCache> = None;
        let mut last_price_fetch: Option<DateTime<Utc>> = None;

        // Give the status loop a brief head start so the first analysis isn't
        // run against an all-zero status (waits up to ~2 poll intervals).
        let wait_deadline = Utc::now() + Duration::from_std(config.status_poll_interval).unwrap_or(Duration::seconds(10)) * 2;
        while latest_status.lock().unwrap_or_else(|e| e.into_inner()).is_none()
            && Utc::now() < wait_deadline
        {
            thread::sleep(time::Duration::from_millis(200));
        }

        if config.enable_heatpump_control {
            if let Err(e) = control_relay(&config, CompressorState::Allowed, &client) {
                let _ = tx.send(Some(AppState::new_with_status(format!(
                    "Startup relay error: {}",
                    e
                ))));
            }
        }

        loop {
            let now = Utc::now();

            // Fetch weather every hour or if missing
            let needs_weather_update = weather_cache.as_ref().map_or(true, |cache| {
                now.signed_duration_since(cache.last_fetched).num_hours() >= 1
            });

            if needs_weather_update {
                match fetch_weather(&config, &client) {
                    Ok(data) => {
                        weather_cache = Some(WeatherCache {
                            forecast: data,
                            last_fetched: now,
                        })
                    }
                    Err(e) => log::error!("Failed to fetch weather: {}", e),
                }
            }

            if config.enable_pricing {
                // Fetch price every hour
                let needs_price_update =
                    last_price_fetch.map_or(true, |t| now.signed_duration_since(t).num_hours() >= 1);

                if needs_price_update {
                    prices.clear();
                    last_price_fetch = Some(now);
                }
            }

            // Fallback empty weather if API fails on startup
            let current_weather = weather_cache
                .as_ref()
                .map(|c| c.forecast.clone())
                .unwrap_or_default();

            // Read the most recent live status published by the fast status loop
            // (refreshed every `status_poll_interval`), rather than fetching here.
            let current_status = latest_status
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
                .unwrap_or_default();

            // Pricing disabled → skip price analysis entirely and just publish
            // the live system status. Heatpump and battery control are forced off
            // upstream, so there are no decisions to render either.
            if !config.enable_pricing {
                let _ = current_weather; // weather still fetched for future enable
                let app_state = AppState {
                    prices: vec![],
                    current_price: 0.0,
                    block_threshold: 0.0,
                    low_price_threshold: 0.0,
                    spike_threshold: 0.0,
                    compressor_decision: CompressorState::Allowed,
                    soc_decision: 0,
                    system_status: current_status,
                    status_message: String::new(),
                    expected_pv_kwh: 0.0,
                    hourly_pv_kw: vec![],
                    hourly_heating_kw: vec![],
                    show_thresholds: false,
                    show_decisions: false,
                };
                if tx.send(Some(app_state)).is_err() {
                    break;
                }
                thread::sleep(config.check_interval);
                continue;
            }

            // Run Analysis
            let result = run_price_analysis(
                &config,
                &mut prices,
                &client,
                now,
                &current_weather,
                &current_status,
            );

            match result {
                Ok((price_based_decision, battery_decision, thresholds, expected_pv_kwh)) => {
                    let mut final_compressor_decision = price_based_decision;

                    // Unpack battery decision
                    let (desired_soc, battery_reason) = match &battery_decision {
                        BatteryDecision::Baseline(soc) => (*soc, "Baseline".to_string()),
                        BatteryDecision::ForceCharge(soc, reason) => {
                            (*soc, format!("Force Charge ({})", reason))
                        }
                        BatteryDecision::WaitForLower(soc, price) => {
                            (*soc, format!("Wait for dip (< {:.2}c)", price))
                        }
                    };

                    let optimized_soc = match battery_decision {
                        BatteryDecision::ForceCharge(target, _) => {
                            // Use calculated target. If target is 45% and battery is 97%,
                            // Victron will use the battery for the house until it hits 45% SOC
                            target
                        }
                        BatteryDecision::WaitForLower(target, _)
                        | BatteryDecision::Baseline(target) => target,
                    };

                    let battery_status_str = if optimized_soc != desired_soc {
                        format!("{} [Holding @ {}%]", battery_reason, current_status.soc)
                    } else {
                        battery_reason
                    };

                    let mut status_message = battery_status_str;

                    if config.enable_heatpump_control {
                        // Reset block timer if price-based decision allows running.
                        if price_based_decision == CompressorState::Allowed {
                            block_start_time = None;
                        }

                        // Timing logic overrides
                        if let Some(rest_end) = rest_until_time {
                            if now < rest_end {
                                status_message = format!(
                                    "Resting until {}",
                                    rest_end.with_timezone(&Tz::CET).format("%H:%M")
                                );
                                final_compressor_decision = CompressorState::Allowed;
                            } else {
                                rest_until_time = None;
                            }
                        }

                        // Check if the maximum block time has been exceeded.
                        if let Some(block_start) = block_start_time {
                            let block_duration = now.signed_duration_since(block_start);
                            if block_duration
                                > Duration::minutes(config.max_continuous_block_minutes)
                            {
                                status_message = format!(
                                    "Max block exceeded ({}m)",
                                    block_duration.num_minutes()
                                );
                                final_compressor_decision = CompressorState::Allowed;
                            }
                        }

                        // Update timers on state change
                        if final_compressor_decision != last_compressor_state {
                            match (last_compressor_state, final_compressor_decision) {
                                (CompressorState::Allowed, CompressorState::Blocked) => {
                                    if block_start_time.is_none() {
                                        block_start_time = Some(now);
                                    }
                                }
                                (CompressorState::Blocked, CompressorState::Allowed) => {
                                    block_start_time = None;
                                    rest_until_time = Some(
                                        now + Duration::minutes(config.min_rest_time_minutes),
                                    );
                                }
                                _ => {}
                            }
                        }

                        // Send commands if state changed
                        if final_compressor_decision != last_compressor_state {
                            if let Err(e) =
                                control_relay(&config, final_compressor_decision, &client)
                            {
                                let _ = tx.send(Some(AppState::new_with_status(format!(
                                    "Relay error: {}",
                                    e
                                ))));
                            }
                        }
                        last_compressor_state = final_compressor_decision;
                    } else {
                        // Heatpump control disabled: surface the price-based decision but take
                        // no action and run no timing state machine.
                        final_compressor_decision = price_based_decision;
                    }

                    // Handle Battery SOC State with OPTIMIZED value
                    if config.enable_battery_control && optimized_soc != last_soc_set {
                        if let Err(e) = set_minimum_soc(&config, optimized_soc) {
                            let _ = tx
                                .send(Some(AppState::new_with_status(format!("SSH error: {}", e))));
                        }
                        last_soc_set = optimized_soc;
                    }

                    // Parse hourly sunshine and heating for the chart
                    let mut hourly_pv_kw = vec![];
                    let mut hourly_heating_kw = vec![];

                    if let Some(cache) = &weather_cache {
                        for (i, t_str) in cache.forecast.hourly.time.iter().enumerate() {
                            if let Ok(ndt) = NaiveDateTime::parse_from_str(t_str, "%Y-%m-%dT%H:%M")
                            {
                                if let Some(ts) = ndt.and_local_timezone(Tz::CET).single() {
                                    // Parse PV
                                    let radiation = cache
                                        .forecast
                                        .hourly
                                        .shortwave_radiation
                                        .get(i)
                                        .copied()
                                        .unwrap_or(0.0);
                                    let pv_kw = config.pv_size_kwp * (radiation / 1000.0) * 0.85;
                                    hourly_pv_kw.push((ts.timestamp() as f64, pv_kw));

                                    // Parse Heating
                                    let temp = cache
                                        .forecast
                                        .hourly
                                        .temperature_2m
                                        .get(i)
                                        .copied()
                                        .unwrap_or(0.0);
                                    let mut kw = 0.0;
                                    if temp < config.heating_off_temp_c {
                                        let temp_factor = (config.heating_off_temp_c - temp)
                                            / config.heating_off_temp_c;
                                        kw = config.heating_kwh_per_h_at_0c * temp_factor;
                                    }
                                    hourly_heating_kw.push((ts.timestamp() as f64, kw));
                                }
                            }
                        }
                    }

                    // Send complete state to UI thread
                    let app_state = AppState {
                        prices: prices.iter().cloned().collect(),
                        current_price: thresholds.current_price,
                        block_threshold: thresholds.block_threshold,
                        low_price_threshold: thresholds.low_price_threshold,
                        spike_threshold: thresholds.spike_threshold,
                        compressor_decision: final_compressor_decision,
                        soc_decision: optimized_soc,
                        expected_pv_kwh: expected_pv_kwh,
                        hourly_pv_kw,
                        hourly_heating_kw: hourly_heating_kw,
                        system_status: current_status,
                        status_message,
                        show_thresholds: config.enable_pricing,
                        show_decisions: config.enable_heatpump_control
                            || config.enable_battery_control,
                    };
                    if tx.send(Some(app_state)).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    // On error, send status and ensure failsafe state
                    if config.enable_heatpump_control
                        && last_compressor_state == CompressorState::Blocked
                    {
                        if control_relay(&config, CompressorState::Allowed, &client).is_ok() {
                            last_compressor_state = CompressorState::Allowed;
                            block_start_time = None;
                        }
                    }
                    if tx
                        .send(Some(AppState::new_with_status(format!("Error: {}", e))))
                        .is_err()
                    {
                        break;
                    }
                }
            }
            thread::sleep(config.check_interval);
        }
    });

    log::info!(
        "Energy controller started; web/MCP server and control loop are running. \
         Logging to stderr (set RUST_LOG=debug for more detail)."
    );

    // The control loop and the web/MCP server run on background threads and
    // report their status through the log. Block here for the process lifetime.
    if let Err(e) = controller.join() {
        log::error!("Control loop thread terminated unexpectedly: {:?}", e);
    }
    Ok(())
}

/// Spawns the fast status-polling loop. It owns the configured data sources and
/// refreshes the live system status every `config.status_poll_interval`, storing
/// it in `latest_status` (consumed by the control loop) and patching the
/// published `AppState` so the web UI / MCP see near-real-time live values
/// without waiting for the slower price/decision control loop.
fn spawn_status_loop(
    config: Arc<Config>,
    tx: tokio::sync::watch::Sender<Option<AppState>>,
    rx: tokio::sync::watch::Receiver<Option<AppState>>,
    latest_status: Arc<Mutex<Option<SystemStatus>>>,
) {
    thread::spawn(move || {
        let client = Client::builder().use_rustls_tls().build().unwrap();

        let mut data_sources = DataSources::new(
            if config.use_legacy_status {
                Some(LegacySource {
                    url: config.status_url.clone(),
                })
            } else {
                None
            },
            if config.use_huawei {
                Some(HuaweiSource::new(
                    config.huawei_api_url.clone(),
                    config.huawei_username.clone(),
                    config.huawei_system_code.clone(),
                    config.huawei_station_code.clone(),
                    config.huawei_invert_grid_sign,
                ))
            } else {
                None
            },
            if config.use_solaredge {
                Some(SolarEdgeSource {
                    api_url: config.solaredge_api_url.clone(),
                    api_key: config.solaredge_api_key.clone(),
                    site_id: config.solaredge_site_id.clone(),
                })
            } else {
                None
            },
            if config.use_live_status {
                Some(LiveSource {
                    url: config.live_status_url.clone(),
                })
            } else {
                None
            },
        );

        if data_sources.legacy.is_none()
            && data_sources.huawei.is_none()
            && data_sources.solaredge.is_none()
            && data_sources.live.is_none()
        {
            log::warn!(
                "No data source enabled. Set at least one of USE_LEGACY_STATUS, USE_HUAWEI, USE_SOLAREDGE, USE_LIVE_STATUS."
            );
        }

        log::info!(
            "Live system status polling every {}s",
            config.status_poll_interval.as_secs()
        );

        loop {
            match data_sources.fetch_status(&client) {
                Ok(status) => {
                    *latest_status.lock().unwrap_or_else(|e| e.into_inner()) =
                        Some(status.clone());
                    // Patch only the live-status portion of the latest published
                    // state, preserving any prices/decisions the control loop set.
                    let mut state = rx
                        .borrow()
                        .clone()
                        .unwrap_or_else(|| AppState::live_bootstrap(&config));
                    state.system_status = status;
                    let _ = tx.send(Some(state));
                }
                Err(e) => log::error!("Failed to fetch system status: {}", e),
            }
            thread::sleep(config.status_poll_interval);
        }
    });
}

struct PriceThresholds {
    current_price: f64,
    block_threshold: f64,
    low_price_threshold: f64,
    spike_threshold: f64,
}

fn calculate_required_soc(
    config: &Config,
    weather: &WeatherResponse,
    start_time: DateTime<Utc>,
    end_time: DateTime<Utc>,
) -> u8 {
    let mut total_kwh = 0.0;
    let mut current = start_time;

    while current < end_time {
        let mut temp_c = 0.0; // Failsafe to 0C if missing
        let mut radiation = 0.0;

        // Open-Meteo time format is "YYYY-MM-DDTHH:00"
        let current_local = current
            .with_timezone(&Tz::CET)
            .format("%Y-%m-%dT%H:00")
            .to_string();

        if let Some(idx) = weather.hourly.time.iter().position(|t| t == &current_local) {
            temp_c = weather
                .hourly
                .temperature_2m
                .get(idx)
                .copied()
                .unwrap_or(0.0);

            radiation = weather
                .hourly
                .shortwave_radiation
                .get(idx)
                .copied()
                .unwrap_or(0.0);
        }

        // Add baseline house load
        total_kwh += config.base_load_kw;

        // Add dynamic heating load
        if temp_c < config.heating_off_temp_c {
            let temp_factor = (config.heating_off_temp_c - temp_c) / config.heating_off_temp_c;
            total_kwh += config.heating_kwh_per_h_at_0c * temp_factor;
        }

        // Subtract expected PV production
        // Panels are rated at 1000 W/m².
        // 0.85 accounts for standard real-world system/inverter losses.
        let expected_pv_kw = config.pv_size_kwp * (radiation / 1000.0) * 0.85;
        total_kwh -= expected_pv_kw;

        current += Duration::hours(1);
    }

    // Ensure total_kwh doesn't drop below 0 (we can't give energy back from the calculation like this)
    total_kwh = f64::max(0.0, total_kwh);

    // Convert required kWh into a percentage of your battery size
    let required_soc_percent = (total_kwh / config.battery_size_kwh) * 100.0;

    // Add baseline SOC so we don't drain to absolute zero
    let target = get_baseline_soc(config, start_time) as f64 + required_soc_percent;

    cmp::min(100, target.ceil() as u8)
}

fn run_price_analysis(
    config: &Config,
    prices: &mut VecDeque<PriceInfo>,
    client: &Client,
    now: DateTime<Utc>,
    weather: &WeatherResponse,
    current_status: &SystemStatus,
) -> Result<(CompressorState, BatteryDecision, PriceThresholds, f64)> {
    // Update our local price cache
    update_price_data(prices, now, client).context("Failed to update price data")?;

    let current_price_info = prices
        .iter()
        .find(|p| now >= p.from && now < p.from + Duration::minutes(15))
        .context("Could not find current price information")?;

    // Sliding Window Threshold Calculation
    // Ensure we look ahead at least 24 hours, or the lookahead window, whichever is larger.
    let analysis_window_hours = cmp::max(24, config.lookahead_hours);
    let window_end = now + Duration::hours(analysis_window_hours);

    let mut window_prices_values: Vec<f64> = prices
        .iter()
        .filter(|p| p.from >= now && p.from < window_end)
        .map(|p| p.price)
        .collect();

    if window_prices_values.len() < 4 {
        return Err(anyhow::anyhow!("Not enough price data for sliding window"));
    }

    let block_threshold = percentile(
        &mut window_prices_values,
        config.block_price_percentile / 100.0,
    );
    let low_price_threshold = percentile(
        &mut window_prices_values,
        config.low_price_percentile / 100.0,
    );
    let spike_threshold = percentile(
        &mut window_prices_values,
        config.high_spike_percentile / 100.0,
    );

    let thresholds = PriceThresholds {
        current_price: current_price_info.price,
        block_threshold,
        low_price_threshold,
        spike_threshold,
    };

    // Heatpump / Relay Decision
    let price_based_decision = if current_price_info.price > block_threshold {
        CompressorState::Blocked
    } else {
        CompressorState::Allowed
    };

    // Initial Battery Decision Logic
    let mut baseline_soc = get_baseline_soc(config, now);
    let force_charge_soc = get_force_charge_soc(config, now);

    // In winter, if next-day solar is predicted to produce at least 50% of battery capacity,
    // lower the discharge floor to the summer minimum (10%) during the morning price spike
    // window (06:00–12:00). Outside that window the winter minimum (20%) is preserved so the
    // extra capacity is held in reserve and only released at the start of the spike.
    let is_winter_baseline = matches!(now.month(), 10..=12 | 1..=3);
    if is_winter_baseline && baseline_soc > config.summer_min_soc {
        let next_day_pv_kwh = calculate_next_day_pv_kwh(config, weather, now);
        let fifty_percent_kwh = config.battery_size_kwh * 0.5;
        if next_day_pv_kwh >= fifty_percent_kwh {
            let local_hour = now.with_timezone(&Tz::CET).hour();
            if local_hour >= 6 && local_hour < 12 {
                log::info!(
                    "Morning spike window ({}h CET): next-day PV forecast ({:.2}kWh) >= 50% \
                     battery capacity ({:.2}kWh). Lowering min SOC from {}% to {}% for discharge.",
                    local_hour,
                    next_day_pv_kwh,
                    fifty_percent_kwh,
                    baseline_soc,
                    config.summer_min_soc
                );
                baseline_soc = config.summer_min_soc;
            }
        }
    }

    let mut battery_decision = BatteryDecision::Baseline(baseline_soc);

    if config.enable_battery_control {
        let lookahead_end_time = now + Duration::hours(config.lookahead_hours);
        let future_prices: Vec<&PriceInfo> = prices
            .iter()
            .filter(|p| p.from >= now && p.from < lookahead_end_time)
            .collect();

        // Check for incoming spikes
        if let Some(max_price_info) = future_prices
            .iter()
            .max_by(|a, b| a.price.partial_cmp(&b.price).unwrap())
        {
            if max_price_info.price > spike_threshold
                && (max_price_info.price - current_price_info.price)
                    > config.min_spike_difference_cents
            {
                let charge_window_end = max_price_info.from;

                // Calculate exactly how much SOC we need for the night + the morning spike (assumed 4 hours)
                let dynamic_target = calculate_required_soc(
                    config,
                    weather,
                    now,
                    charge_window_end + Duration::hours(4),
                );

                let mut dip_prices: Vec<f64> = prices
                    .iter()
                    .filter(|p| p.from >= now && p.from < charge_window_end)
                    .map(|p| p.price)
                    .collect();

                if !dip_prices.is_empty() {
                    let dip_entry_threshold = percentile(&mut dip_prices, 0.25);
                    if current_price_info.price <= dip_entry_threshold {
                        battery_decision = BatteryDecision::ForceCharge(
                            dynamic_target,
                            format!("Pre-Spike (Need {}%)", dynamic_target),
                        );
                    } else {
                        battery_decision =
                            BatteryDecision::WaitForLower(baseline_soc, dip_entry_threshold);
                    }
                } else {
                    battery_decision = BatteryDecision::ForceCharge(
                        dynamic_target,
                        format!("Immediate Spike (Need {}%)", dynamic_target),
                    );
                }
            }
        }

        // If no spike is detected, check if we are in a general low-price dip
        if matches!(battery_decision, BatteryDecision::Baseline(_))
            && current_price_info.price < low_price_threshold
        {
            if let Some(min_future_price) = future_prices
                .iter()
                .map(|p| p.price)
                .min_by(|a, b| a.partial_cmp(b).unwrap())
            {
                if current_price_info.price <= min_future_price + 0.01 {
                    battery_decision =
                        BatteryDecision::ForceCharge(force_charge_soc, "Low Point".to_string());
                } else {
                    battery_decision =
                        BatteryDecision::WaitForLower(baseline_soc, min_future_price);
                }
            } else {
                // No future prices available in window, force charge as an End-Of-Day low
                battery_decision =
                    BatteryDecision::ForceCharge(force_charge_soc, "EOD Low".to_string());
            }
        }

        // PV Forecast Overrides
        let missing_soc_percent = 100.0 - (current_status.soc as f64);
        let missing_kwh = config.battery_size_kwh * (missing_soc_percent / 100.0);

        // Calculate expected PV for the REST OF TODAY using accurate W/m² radiation
        let mut expected_pv_kwh = 0.0;
        let today = now.with_timezone(&Tz::CET).date_naive();

        for (i, t_str) in weather.hourly.time.iter().enumerate() {
            if let Ok(ndt) = NaiveDateTime::parse_from_str(t_str, "%Y-%m-%dT%H:%M") {
                if let Some(ts) = ndt.and_local_timezone(Tz::CET).single() {
                    // Only sum up the energy for hours that are still coming up today
                    if ts >= now && ts.date_naive() == today {
                        let radiation = weather
                            .hourly
                            .shortwave_radiation
                            .get(i)
                            .copied()
                            .unwrap_or(0.0);

                        // Panels are rated at 1000 W/m².
                        // 0.85 accounts for standard real-world system/inverter losses.
                        let expected_kw = config.pv_size_kwp * (radiation / 1000.0) * 0.85;

                        // 1 hour of expected_kw = expected_kwh
                        expected_pv_kwh += expected_kw;
                    }
                }
            }
        }

        // Favor PV independent of season.
        if expected_pv_kwh >= missing_kwh {
            // The sun will fill the battery. Cancel grid ForceCharge unless prices are basically zero.
            if let BatteryDecision::ForceCharge(_, reason) = &battery_decision {
                let reason_clone = reason.clone();
                if current_price_info.price > 1.0 {
                    battery_decision = BatteryDecision::Baseline(baseline_soc);
                    log::info!(
                        "Canceled ForceCharge ({}). PV yield expected: {:.2}kWh, missing: {:.2}kWh",
                        reason_clone,
                        expected_pv_kwh,
                        missing_kwh
                    );
                }
            }
        } else {
            // If expected PV is terrible and price is somewhat low, aggressively force charge.
            let is_winter = matches!(now.month(), 10..=12 | 1..=3);
            if is_winter {
                if expected_pv_kwh < (missing_kwh * 0.3)
                    && current_price_info.price < low_price_threshold
                {
                    battery_decision = BatteryDecision::ForceCharge(
                        force_charge_soc,
                        "Winter Low PV Grid Charge".to_string(),
                    );
                }
            }
        }

        return Ok((
            price_based_decision,
            battery_decision,
            thresholds,
            expected_pv_kwh,
        ));
    }

    // Fallback if battery control is disabled
    Ok((price_based_decision, battery_decision, thresholds, 0.0))
}

impl AppState {
    fn new_with_status(status: String) -> Self {
        AppState {
            prices: vec![],
            current_price: 0.0,
            block_threshold: 0.0,
            low_price_threshold: 0.0,
            spike_threshold: 0.0,
            compressor_decision: CompressorState::Allowed,
            soc_decision: 0,
            system_status: SystemStatus::default(),
            status_message: status,
            expected_pv_kwh: 0.0,
            hourly_pv_kw: vec![],
            hourly_heating_kw: vec![],
            // Conservative defaults: only used briefly on error paths, when
            // panels we'd otherwise hide are likely the source of the error
            // and the user wants to see the message.
            show_thresholds: true,
            show_decisions: true,
        }
    }

    /// Minimal state used by the status loop before the control loop has
    /// produced a full one: carries only live status, with panel visibility
    /// matching the running mode.
    fn live_bootstrap(config: &Config) -> Self {
        let mut state = AppState::new_with_status(String::new());
        state.show_thresholds = config.enable_pricing;
        state.show_decisions =
            config.enable_heatpump_control || config.enable_battery_control;
        state
    }
}

/// Determines the baseline Minimum SOC based on the season.
fn get_baseline_soc(config: &Config, now: DateTime<Utc>) -> u8 {
    match now.month() {
        4..=9 => config.summer_min_soc, // April to September is Summer
        _ => config.winter_min_soc,     // October to March is Winter
    }
}

/// Calculates the total expected PV production (kWh) for the next calendar day.
fn calculate_next_day_pv_kwh(config: &Config, weather: &WeatherResponse, now: DateTime<Utc>) -> f64 {
    let tomorrow = (now.with_timezone(&Tz::CET) + Duration::days(1)).date_naive();
    let mut total_kwh = 0.0;

    for (i, t_str) in weather.hourly.time.iter().enumerate() {
        if let Ok(ndt) = NaiveDateTime::parse_from_str(t_str, "%Y-%m-%dT%H:%M") {
            if let Some(ts) = ndt.and_local_timezone(Tz::CET).single() {
                if ts.date_naive() == tomorrow {
                    let radiation = weather
                        .hourly
                        .shortwave_radiation
                        .get(i)
                        .copied()
                        .unwrap_or(0.0);
                    let expected_kw = config.pv_size_kwp * (radiation / 1000.0) * 0.85;
                    total_kwh += expected_kw;
                }
            }
        }
    }

    total_kwh
}
fn get_force_charge_soc(config: &Config, now: DateTime<Utc>) -> u8 {
    match now.month() {
        4..=9 => config.summer_force_charge_soc,
        _ => config.winter_force_charge_soc,
    }
}

fn set_minimum_soc(config: &Config, soc: u8) -> Result<()> {
    log::info!("Set Minimum SOC to {}% on host {}", soc, config.ssh_host);
    let tcp = TcpStream::connect(format!("{}:22", config.ssh_host))
        .with_context(|| format!("Failed to connect to SSH host {}", config.ssh_host))?;
    let mut sess = Session::new()?;
    sess.set_tcp_stream(tcp);
    sess.handshake()?;
    sess.userauth_password(&config.ssh_user, &config.ssh_pass)
        .context("SSH authentication failed. Check user/password.")?;

    if !sess.authenticated() {
        return Err(anyhow::anyhow!("SSH authentication failed silently."));
    }

    let mut channel = sess.channel_session()?;
    let command = format!(
        "dbus-send --system --print-reply --dest=com.victronenergy.settings \
        /Settings/CGwacs/BatteryLife/MinimumSocLimit \
        com.victronenergy.BusItem.SetValue variant:int32:{}",
        soc
    );
    log::debug!("Executing SSH command: {}", command);
    channel.exec(&command)?;

    let mut output = String::new();
    channel.read_to_string(&mut output)?;
    log::debug!("SSH command stdout: {}", output);

    channel.wait_close()?;
    let exit_status = channel.exit_status()?;
    if exit_status != 0 {
        return Err(anyhow::anyhow!(
            "SSH command failed with exit status {}",
            exit_status
        ));
    }

    log::debug!("SSH command successful (exit code {}).", exit_status);
    Ok(())
}

/// Manages the local price data cache, fetching new data when needed.
fn update_price_data(
    prices: &mut VecDeque<PriceInfo>,
    now: DateTime<Utc>,
    client: &Client,
) -> Result<()> {
    // Remove old price data from the front of the queue.
    let today = now.with_timezone(&Tz::CET).date_naive();
    while let Some(front) = prices.front() {
        if front.from.with_timezone(&Tz::CET).date_naive() < today {
            prices.pop_front();
        } else {
            break;
        }
    }

    // Check if we need to fetch data for today or tomorrow.
    let today = now.date_naive();
    let tomorrow = today
        .succ_opt()
        .context("Failed to calculate tomorrow's date")?;

    if !prices.iter().any(|p| p.from.date_naive() == today) {
        log::debug!("Fetching price data for today ({})", today);
        prices.extend(
            fetch_prices_for_day(today, client)
                .with_context(|| format!("Failed to fetch prices for {}", today))?,
        );
    }
    if !prices.iter().any(|p| p.from.date_naive() == tomorrow) {
        log::debug!("Fetching price data for tomorrow ({})", tomorrow);
        prices.extend(
            fetch_prices_for_day(tomorrow, client)
                .with_context(|| format!("Failed to fetch prices for {}", tomorrow))?,
        );
    }

    // Sort the combined list by date to ensure it's in chronological order.
    // This is important because fetches for today/tomorrow might complete out of order.
    let mut sorted_vec: Vec<_> = prices.drain(..).collect();
    sorted_vec.sort_by_key(|p| p.from);
    *prices = sorted_vec.into();
    Ok(())
}

/// Fetches and deserializes price data for a specific day from the API.
fn fetch_prices_for_day(day: NaiveDate, client: &Client) -> Result<Vec<PriceInfo>> {
    let url = format!(
        "https://i.spottyenergie.at/api/prices/public/{}",
        day.format("%Y-%m-%d")
    );
    let response = client.get(&url).send()?;
    if !response.status().is_success() {
        return Err(anyhow::anyhow!(
            "API request failed with status: {}",
            response.status()
        ));
    }
    Ok(response.json()?)
}

/// Sends an HTTP request to the Shelly relay to turn it on or off.
fn control_relay(config: &Config, desired_state: CompressorState, client: &Client) -> Result<()> {
    let action = match (desired_state, config.relay_on_to_block) {
        (CompressorState::Blocked, true) => "on",
        (CompressorState::Allowed, true) => "off",
        (CompressorState::Blocked, false) => "off",
        (CompressorState::Allowed, false) => "on",
    };
    let url = format!("http://{}/relay/0?turn={}", config.shelly_ip, action);
    log::debug!("Sending request to Shelly: {}", url);
    let response = client.get(&url).send()?;
    if !response.status().is_success() {
        return Err(anyhow::anyhow!(
            "Shelly device responded with status: {}",
            response.status()
        ));
    }
    Ok(())
}

fn fetch_weather(config: &Config, client: &Client) -> Result<WeatherResponse> {
    // Fetches today and tomorrow automatically based on timezone
    let url = format!(
        "https://api.open-meteo.com/v1/forecast?latitude={}&longitude={}&hourly=temperature_2m,shortwave_radiation&forecast_days=2&timezone=auto",
        config.lat, config.lon
    );

    log::debug!("Fetching weather data from Open-Meteo...");
    let response = client.get(&url).send()?;

    if !response.status().is_success() {
        return Err(anyhow::anyhow!(
            "Open-Meteo API failed: {}",
            response.status()
        ));
    }

    Ok(response.json()?)
}

fn parse_bool(name: &str, default: &str) -> Result<bool> {
    Ok(env::var(name).unwrap_or(default.to_string()).parse()?)
}

/// Loads configuration from environment variables.
fn load_config() -> Result<Config> {
    let use_legacy_status = parse_bool("USE_LEGACY_STATUS", "true")?;
    let use_huawei = parse_bool("USE_HUAWEI", "false")?;
    let use_solaredge = parse_bool("USE_SOLAREDGE", "false")?;
    let use_live_status = parse_bool("USE_LIVE_STATUS", "false")?;
    let enable_pricing = parse_bool("USE_PRICING", "true")?;
    let mut enable_heatpump_control = parse_bool("ENABLE_HEATPUMP_CONTROL", "true")?;
    let mut enable_battery_control = parse_bool("ENABLE_BATTERY_CONTROL", "true")?;
    if !enable_pricing {
        if enable_heatpump_control || enable_battery_control {
            log::warn!(
                "USE_PRICING=false: forcing ENABLE_HEATPUMP_CONTROL and ENABLE_BATTERY_CONTROL off (both require price data)."
            );
        }
        enable_heatpump_control = false;
        enable_battery_control = false;
    }

    let huawei_username = if use_huawei {
        env::var("HUAWEI_USERNAME").context("HUAWEI_USERNAME not set (required when USE_HUAWEI=true)")?
    } else {
        env::var("HUAWEI_USERNAME").unwrap_or_default()
    };
    let huawei_system_code = if use_huawei {
        env::var("HUAWEI_SYSTEM_CODE")
            .context("HUAWEI_SYSTEM_CODE not set (required when USE_HUAWEI=true)")?
    } else {
        env::var("HUAWEI_SYSTEM_CODE").unwrap_or_default()
    };
    let solaredge_api_key = if use_solaredge {
        env::var("SOLAREDGE_API_KEY")
            .context("SOLAREDGE_API_KEY not set (required when USE_SOLAREDGE=true)")?
    } else {
        env::var("SOLAREDGE_API_KEY").unwrap_or_default()
    };
    let solaredge_site_id = if use_solaredge {
        env::var("SOLAREDGE_SITE_ID")
            .context("SOLAREDGE_SITE_ID not set (required when USE_SOLAREDGE=true)")?
    } else {
        env::var("SOLAREDGE_SITE_ID").unwrap_or_default()
    };

    Ok(Config {
        enable_pricing,
        use_legacy_status,
        use_huawei,
        use_solaredge,
        use_live_status,
        status_url: env::var("STATUS_URL")
            .unwrap_or("http://192.168.178.11/status/soc.txt".to_string()),
        live_status_url: env::var("LIVE_STATUS_URL")
            .unwrap_or("https://hoxdna.org/getEnergy".to_string()),
        huawei_api_url: env::var("HUAWEI_API_URL")
            .unwrap_or("https://eu5.fusionsolar.huawei.com".to_string()),
        huawei_username,
        huawei_system_code,
        huawei_station_code: env::var("HUAWEI_STATION_CODE").ok().filter(|s| !s.is_empty()),
        huawei_invert_grid_sign: parse_bool("HUAWEI_INVERT_GRID_SIGN", "false")?,
        solaredge_api_url: env::var("SOLAREDGE_API_URL")
            .unwrap_or("https://monitoringapi.solaredge.com".to_string()),
        solaredge_api_key,
        solaredge_site_id,
        enable_heatpump_control,
        shelly_ip: if enable_heatpump_control {
            env::var("SHELLY_IP").context("SHELLY_IP not set (required when ENABLE_HEATPUMP_CONTROL=true)")?
        } else {
            env::var("SHELLY_IP").unwrap_or_default()
        },
        relay_on_to_block: env::var("RELAY_ON_TO_BLOCK")
            .unwrap_or("false".into())
            .parse()?,
        check_interval: time::Duration::from_secs(
            env::var("CHECK_INTERVAL_MINUTES")
                .unwrap_or("5".into())
                .parse::<u64>()?
                * 60,
        ),
        // How often the live system status (SOC/PV/load/grid/battery) is
        // refreshed, independent of the slower price/decision control loop.
        // Keep this within your data source's rate limits — e.g. SolarEdge's
        // cloud API allows only ~300 requests/day, so do NOT poll it every 10s.
        status_poll_interval: time::Duration::from_secs(
            env::var("STATUS_POLL_SECONDS")
                .unwrap_or("10".into())
                .parse::<u64>()?
                .max(1),
        ),
        block_price_percentile: env::var("BLOCK_PRICE_PERCENTILE")
            .unwrap_or("75.0".into())
            .parse()?,
        max_continuous_block_minutes: env::var("MAX_CONTINUOUS_BLOCK_MINUTES")
            .unwrap_or("120".into())
            .parse()?,
        min_rest_time_minutes: env::var("MIN_REST_TIME_MINUTES")
            .unwrap_or("60".into())
            .parse()?,
        lookahead_hours: env::var("LOOKAHEAD_HOURS").unwrap_or("6".into()).parse()?,
        low_price_percentile: env::var("LOW_PRICE_PERCENTILE")
            .unwrap_or("10.0".into())
            .parse()?,
        high_spike_percentile: env::var("HIGH_SPIKE_PERCENTILE")
            .unwrap_or("95.0".into())
            .parse()?,
        min_spike_difference_cents: env::var("MIN_SPIKE_DIFFERENCE_CENTS")
            .unwrap_or("10.0".into())
            .parse()?,
        enable_battery_control,
        ssh_host: env::var("SSH_HOST").context("SSH_HOST not set")?,
        ssh_user: env::var("SSH_USER").context("SSH_USER not set")?,
        ssh_pass: env::var("SSH_PASS").context("SSH_PASS not set")?,
        winter_force_charge_soc: env::var("WINTER_FORCE_CHARGE_SOC")
            .unwrap_or("60".into())
            .parse()?,
        summer_force_charge_soc: env::var("SUMMER_FORCE_CHARGE_SOC")
            .unwrap_or("30".into())
            .parse()?,
        summer_min_soc: env::var("SUMMER_MIN_SOC").unwrap_or("10".into()).parse()?,
        winter_min_soc: env::var("WINTER_MIN_SOC").unwrap_or("20".into()).parse()?,
        lat: env::var("LATITUDE").unwrap_or("48.2082".into()).parse()?,
        lon: env::var("LONGITUDE").unwrap_or("16.3738".into()).parse()?,
        battery_size_kwh: env::var("BATTERY_SIZE_KWH")
            .unwrap_or("10.0".into())
            .parse()?,
        pv_size_kwp: env::var("PV_SIZE_KWP").unwrap_or("5.0".into()).parse()?,
        base_load_kw: env::var("BASE_LOAD_KW").unwrap_or("0.5".into()).parse()?, // 500W background load
        heating_off_temp_c: env::var("HEATING_OFF_TEMP_C")
            .unwrap_or("15.0".into())
            .parse()?, // Temp where heating stops
        heating_kwh_per_h_at_0c: env::var("HEATING_KWH_PER_H_AT_0C")
            .unwrap_or("1.5".into())
            .parse()?,
    })
}

/// Calculates the percentile value from a slice of f64 values.
/// `percentile` should be between 0.0 and 1.0 (e.g., 0.75 for 75th percentile).
fn percentile(data: &mut [f64], percentile: f64) -> f64 {
    if data.is_empty() {
        return 0.0;
    }
    data.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let rank = percentile * (data.len() - 1) as f64;
    let lower = rank.floor() as usize;
    let upper = rank.ceil() as usize;
    if lower == upper {
        data[lower]
    } else {
        let weight = rank - lower as f64;
        data[lower] * (1.0 - weight) + data[upper] * weight
    }
}
