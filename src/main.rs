use anyhow::{Context, Result};
use chrono::{Datelike, DateTime, Duration, NaiveDate, Utc};
use chrono_tz::Tz;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use reqwest::blocking::Client;
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    symbols,
    text::{Span, Line},
    widgets::{Axis, Block, Borders, Chart, Dataset, GraphType, Paragraph},
    Frame, Terminal,
    prelude::Alignment,
};
use serde::Deserialize;
use ssh2::Session;
use std::{
    cmp,
    collections::VecDeque,
    env,
    io::{self, Read},
    net::TcpStream,
    sync::mpsc,
    thread,
    time,
};

/// Represents a single price point from the API.
#[derive(Deserialize, Debug, Clone)]
struct PriceInfo {
    from: DateTime<Utc>,
    price: f64,
}

/// Represents the live status of the house/battery system.
#[derive(Debug, Clone, Default)]
struct SystemStatus {
    soc: u8,
    pv_power: f64,
    load_power: f64,
    grid_power: f64,
    battery_power: f64,
}

/// Represents the desired state of the heatpump compressor.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
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

/// Holds all configuration loaded from environment variables.
struct Config {
    // General
    check_interval: time::Duration,
    status_url: String,
    // Relay settings
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
}

/// Holds all the data needed for the UI to render a frame.
#[derive(Clone, Debug)]
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
}


fn main() -> Result<()> {
    dotenv::dotenv().ok();
    let config = load_config()?;
    let (tx, rx) = mpsc::channel::<AppState>(); 
    
    thread::spawn(move || {
        let client = Client::builder().use_rustls_tls().build().unwrap();
        let mut prices: VecDeque<PriceInfo> = VecDeque::new();
        let mut last_compressor_state = CompressorState::Allowed;
        let mut last_soc_set: u8 = 0;
        let mut block_start_time: Option<DateTime<Utc>> = None;
        let mut rest_until_time: Option<DateTime<Utc>> = None;

        if let Err(e) = control_relay(&config, CompressorState::Allowed, &client) {
            let _ = tx.send(AppState::new_with_status(format!("Startup relay error: {}", e)));
        }

        loop {
            let now = Utc::now();
            
            // Fetch current system status (SOC, PV, etc.)
            let current_status = fetch_system_status(&client, &config.status_url)
                .unwrap_or_else(|e| {
                    log::error!("Failed to fetch system status: {}", e);
                    SystemStatus::default()
                });

            // Run Analysis
            let result = run_price_analysis(&config, &mut prices, &client, now);

            match result {
                Ok((price_based_decision, battery_decision, thresholds)) => {
                    let mut final_compressor_decision = price_based_decision;

                    // Unpack battery decision
                    let (desired_soc, battery_reason) = match &battery_decision {
                        BatteryDecision::Baseline(soc) => (*soc, "Baseline".to_string()),
                        BatteryDecision::ForceCharge(soc, reason) => (*soc, format!("Force Charge ({})", reason)),
                        BatteryDecision::WaitForLower(soc, price) => (*soc, format!("Wait for dip (< {:.2}c)", price)),
                    };

                    let optimized_soc = match battery_decision {
                        BatteryDecision::ForceCharge(target, _) | BatteryDecision::WaitForLower(target, _) => {
                            // If currently higher than target, hold current
                            cmp::max(target, current_status.soc)
                        },
                        BatteryDecision::Baseline(target) => target,
                    };
                    
                    let battery_status_str = if optimized_soc != desired_soc {
                         format!("{} [Holding @ {}%]", battery_reason, current_status.soc)
                    } else {
                        battery_reason
                    };

                    let mut status_message = battery_status_str;

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
                        if block_duration > Duration::minutes(config.max_continuous_block_minutes)
                        {
                            status_message =
                                format!("Max block exceeded ({}m)", block_duration.num_minutes());
                            final_compressor_decision = CompressorState::Allowed;
                        }
                    }

                    // Update timers on state change
                    if final_compressor_decision != last_compressor_state {
                        match (last_compressor_state, final_compressor_decision) {
                            (CompressorState::Allowed, CompressorState::Blocked) => {
                                // Only start a new block timer if we are not already in a block
                                if block_start_time.is_none() {
                                    block_start_time = Some(now);
                                }
                            }
                            (CompressorState::Blocked, CompressorState::Allowed) => {
                                block_start_time = None;
                                rest_until_time =
                                    Some(now + Duration::minutes(config.min_rest_time_minutes));
                            }
                            _ => {}
                        }
                    }

                    // Send commands if state changed
                    if final_compressor_decision != last_compressor_state {
                        if let Err(e) = control_relay(&config, final_compressor_decision, &client)
                        {
                            let _ =
                                tx.send(AppState::new_with_status(format!("Relay error: {}", e)));
                        }
                    }
                    last_compressor_state = final_compressor_decision;

                    // Handle Battery SOC State with OPTIMIZED value
                    if config.enable_battery_control && optimized_soc != last_soc_set {
                        if let Err(e) = set_minimum_soc(&config, optimized_soc) {
                            let _ = tx.send(AppState::new_with_status(format!("SSH error: {}", e)));
                        }
                        last_soc_set = optimized_soc;
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
                        system_status: current_status,
                        status_message,
                    };
                    if tx.send(app_state).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    // On error, send status and ensure failsafe state
                    if last_compressor_state == CompressorState::Blocked {
                        if control_relay(&config, CompressorState::Allowed, &client).is_ok() {
                            last_compressor_state = CompressorState::Allowed;
                            block_start_time = None;
                        }
                    }
                    if tx.send(AppState::new_with_status(format!("Error: {}", e))).is_err() {
                        break;
                    }
                }
            }
            thread::sleep(config.check_interval);
        }
    });

    // Setup TUI
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Run UI Loop
    let mut app_state: Option<AppState> = None;
    loop {
        if let Ok(new_state) = rx.try_recv() {
            app_state = Some(new_state);
        }
        terminal.draw(|f| ui(f, app_state.as_ref()))?;
        if crossterm::event::poll(time::Duration::from_millis(200))? {
            if let Event::Key(key) = event::read()? {
                if key.code == KeyCode::Char('q') {
                    break;
                }
            }
        }
    }

    // Restore Terminal
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;
    terminal.show_cursor()?;
    Ok(())
}

// UI Rendering

fn ui(f: &mut Frame, app_state: Option<&AppState>) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)].as_ref())
        .split(f.area());
    
    let bottom_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(33),
            Constraint::Percentage(33),
            Constraint::Percentage(34)
        ].as_ref())
        .split(chunks[1]);

    if let Some(state) = app_state {
        let (x_bounds, y_bounds, price_data, v_line_data, h_line_data) = prepare_chart_data(state);
        let datasets = vec![
            Dataset::default()
                .name("Price")
                .marker(symbols::Marker::Dot)
                .graph_type(GraphType::Line)
                .style(Style::default().fg(Color::Cyan))
                .data(&price_data),
            Dataset::default()
                .name("Current")
                .marker(symbols::Marker::Braille)
                .graph_type(GraphType::Line)
                .style(Style::default().fg(Color::LightMagenta).add_modifier(Modifier::DIM))
                .data(&h_line_data),
            Dataset::default()
                .name("Now")
                .marker(symbols::Marker::Braille)
                .graph_type(GraphType::Line)
                .style(Style::default().fg(Color::Yellow))
                .data(&v_line_data),
        ];

        let now_str = Utc::now().with_timezone(&Tz::CET).format("%H:%M:%S").to_string();
        let current_info_str = format!("Now: {} @ {:.2}c", now_str, state.current_price);

        let chart = Chart::new(datasets)
            .block(
                Block::default()
                    .title_top(Line::from("Price Data").alignment(Alignment::Left))
                    .title_top(Line::from(current_info_str).alignment(Alignment::Right))
                    .borders(Borders::ALL),
            )
            .x_axis(
                Axis::default()
                    .title("Time")
                    .style(Style::default().fg(Color::Gray))
                    .bounds(x_bounds)
                    .labels(
                        // FIXED: Added type annotation ::<Vec<Span>>
                        x_bounds.iter().step_by(8).map(|&t| 
                            Span::from(DateTime::from_timestamp(t as i64, 0).unwrap().with_timezone(&Tz::CET).format("%H:%M").to_string())
                        ).collect::<Vec<Span>>()
                    ),
            )
            .y_axis(
                Axis::default()
                    .title("c/kWh")
                    .style(Style::default().fg(Color::Gray))
                    .bounds(y_bounds)
                    .labels(y_bounds.iter().map(|&p| Span::from(format!("{:.1}", p))).collect::<Vec<Span>>()),
            );
        f.render_widget(chart, chunks[0]);
    } else {
        f.render_widget(Block::default().title("Loading...").borders(Borders::ALL), chunks[0]);
    }

    // Block 1: Thresholds
    let percentile_block = Block::default().title("Thresholds").borders(Borders::ALL);
    let mut percentile_text = vec![];
    if let Some(state) = app_state {
        percentile_text.push(Line::from(vec![Span::raw("Current: "), Span::styled(format!("{:.2}c", state.current_price), Style::default().fg(Color::White).add_modifier(Modifier::BOLD))]));
        percentile_text.push(Line::from(vec![Span::raw("Low < "), Span::styled(format!("{:.2}c", state.low_price_threshold), Style::default().fg(Color::Green))]));
        percentile_text.push(Line::from(vec![Span::raw("Block > "), Span::styled(format!("{:.2}c", state.block_threshold), Style::default().fg(Color::Rgb(255, 165, 0)))]));
        percentile_text.push(Line::from(vec![Span::raw("Spike > "), Span::styled(format!("{:.2}c", state.spike_threshold), Style::default().fg(Color::Red))]));
    }
    f.render_widget(Paragraph::new(percentile_text).block(percentile_block), bottom_chunks[0]);

    // Block 2: Live System Status (NEW)
    let status_block = Block::default().title("Live System Status").borders(Borders::ALL);
    let mut status_text = vec![];
    if let Some(state) = app_state {
        status_text.push(Line::from(vec![
            Span::raw("SOC: "), 
            Span::styled(format!("{}%", state.system_status.soc), Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))
        ]));
        status_text.push(Line::from(vec![
            Span::raw("PV: "), 
            Span::styled(format!("{:.1} kW", state.system_status.pv_power), Style::default().fg(Color::Yellow))
        ]));
        status_text.push(Line::from(vec![
            Span::raw("Load: "), 
            Span::styled(format!("{:.1} kW", state.system_status.load_power), Style::default().fg(Color::Blue))
        ]));
         status_text.push(Line::from(vec![
            Span::raw("Grid: "), 
            Span::styled(format!("{:.1} kW", state.system_status.grid_power), Style::default().fg(if state.system_status.grid_power > 0.0 { Color::Red } else { Color::Green }))
        ]));
        status_text.push(Line::from(vec![
            Span::raw("Batt: "), 
            Span::styled(format!("{:.1} kW", state.system_status.battery_power), Style::default().fg(if state.system_status.battery_power < 0.0 { Color::Red } else { Color::Green }))
        ]));
    }
    f.render_widget(Paragraph::new(status_text).block(status_block), bottom_chunks[1]);

    // Block 3: Logic Decisions
    let decision_block = Block::default().title("Decisions").borders(Borders::ALL);
    let mut decision_text = vec![];
    if let Some(state) = app_state {
        let (comp_text, comp_style) = match state.compressor_decision {
            CompressorState::Allowed => ("Allowed", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
            CompressorState::Blocked => ("BLOCKED", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
        };
        decision_text.push(Line::from(vec![Span::raw("Heatpump: "), Span::styled(comp_text, comp_style)]));
        decision_text.push(Line::from(vec![
            Span::raw("Set Min SOC: "), 
            Span::styled(format!("{}%", state.soc_decision), Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))
        ]));
        decision_text.push(Line::from(Span::raw(" ")));
        // Split status message if too long for box
        let status_lines: Vec<&str> = state.status_message.split(" [").collect();
        decision_text.push(Line::from(Span::styled(status_lines[0], Style::default().fg(Color::Yellow))));
        if status_lines.len() > 1 {
             decision_text.push(Line::from(Span::styled(format!("[{}", status_lines[1]), Style::default().fg(Color::Yellow))));
        }
    }
    f.render_widget(Paragraph::new(decision_text).block(decision_block), bottom_chunks[2]);
}

fn prepare_chart_data(state: &AppState) -> ([f64; 2], [f64; 2], Vec<(f64, f64)>, Vec<(f64, f64)>, Vec<(f64, f64)>) {
    let now = Utc::now();
    let today = now.with_timezone(&Tz::CET).date_naive();
    let tomorrow = today.succ_opt().unwrap_or(today);

    let relevant_prices: Vec<_> = state.prices.iter().filter(|p| {
        let p_date = p.from.with_timezone(&Tz::CET).date_naive();
        p_date == today || p_date == tomorrow
    }).collect();

    if relevant_prices.is_empty() { return ([0.0, 1.0], [0.0, 1.0], vec![], vec![], vec![]); }

    let price_data: Vec<(f64, f64)> = relevant_prices.iter().map(|p| (p.from.timestamp() as f64, p.price)).collect();
    let min_price = relevant_prices.iter().map(|p| p.price).fold(f64::INFINITY, f64::min);
    let max_price = relevant_prices.iter().map(|p| p.price).fold(f64::NEG_INFINITY, f64::max);
    let y_bounds = [min_price * 0.95, max_price * 1.05];
    let x_min = relevant_prices.first().unwrap().from.timestamp() as f64;
    let x_max = relevant_prices.last().unwrap().from.timestamp() as f64;
    let x_bounds = [x_min, x_max];
    let now_ts = now.timestamp() as f64;
    let v_line_data = vec![(now_ts, y_bounds[0]), (now_ts, y_bounds[1])];
    let h_line_data = vec![(x_bounds[0], state.current_price), (x_bounds[1], state.current_price)];

    (x_bounds, y_bounds, price_data, v_line_data, h_line_data)
}

struct PriceThresholds {
    current_price: f64,
    block_threshold: f64,
    low_price_threshold: f64,
    spike_threshold: f64,
}

fn run_price_analysis(
    config: &Config,
    prices: &mut VecDeque<PriceInfo>,
    client: &Client,
    now: DateTime<Utc>,
) -> Result<(CompressorState, BatteryDecision, PriceThresholds)> {
    update_price_data(prices, now, client).context("Failed to update price data")?;

    let current_price_info = prices.iter().find(|p| now >= p.from && now < p.from + Duration::minutes(15))
        .context("Could not find current price information")?;
    let mut todays_prices_values: Vec<f64> = prices.iter().filter(|p| p.from.date_naive() == now.date_naive())
        .map(|p| p.price).collect();
    if todays_prices_values.len() < 4 { return Err(anyhow::anyhow!("Not enough price data")); }

    let block_threshold = percentile(&mut todays_prices_values, config.block_price_percentile / 100.0);
    let low_price_threshold = percentile(&mut todays_prices_values, config.low_price_percentile / 100.0);
    let spike_threshold = percentile(&mut todays_prices_values, config.high_spike_percentile / 100.0);

    let thresholds = PriceThresholds { current_price: current_price_info.price, block_threshold, low_price_threshold, spike_threshold };
    let price_based_decision = if current_price_info.price > block_threshold { CompressorState::Blocked } else { CompressorState::Allowed };

    let baseline_soc = get_baseline_soc(config, now);
    let force_charge_soc = get_force_charge_soc(config, now);
    let mut battery_decision = BatteryDecision::Baseline(baseline_soc);

    if config.enable_battery_control {
        let lookahead_end_time = now + Duration::hours(config.lookahead_hours);
        let future_prices: Vec<&PriceInfo> = prices.iter().filter(|p| p.from >= now && p.from < lookahead_end_time).collect();

        if let Some(max_price_info) = future_prices.iter().max_by(|a, b| a.price.partial_cmp(&b.price).unwrap()) {
            if max_price_info.price > spike_threshold && (max_price_info.price - current_price_info.price) > config.min_spike_difference_cents {
                let charge_window_end = max_price_info.from;
                let mut dip_prices: Vec<f64> = prices.iter().filter(|p| p.from >= now && p.from < charge_window_end).map(|p| p.price).collect();
                if !dip_prices.is_empty() {
                    let dip_entry_threshold = percentile(&mut dip_prices, 0.25);
                    if current_price_info.price <= dip_entry_threshold {
                        battery_decision = BatteryDecision::ForceCharge(force_charge_soc, "Pre-Spike Dip".to_string());
                    } else {
                        battery_decision = BatteryDecision::WaitForLower(baseline_soc, dip_entry_threshold);
                    }
                } else {
                    // Spike is the very next slot
                    battery_decision = BatteryDecision::ForceCharge(
force_charge_soc,
"Immediate Spike".to_string(),
);
                }
            }
        }

        if matches!(battery_decision, BatteryDecision::Baseline(_)) && current_price_info.price < low_price_threshold {
            if let Some(min_future_price) = future_prices.iter().map(|p| p.price).min_by(|a, b| a.partial_cmp(b).unwrap()) {
                if current_price_info.price <= min_future_price + 0.01 {
                    battery_decision = BatteryDecision::ForceCharge(force_charge_soc, "Low Point".to_string());
                } else {
                    battery_decision = BatteryDecision::WaitForLower(baseline_soc, min_future_price);
                }
            } else {
                // No future prices available in window
                battery_decision =
                    BatteryDecision::ForceCharge(force_charge_soc, "EOD Low".to_string());
            }
        }
    }
    Ok((price_based_decision, battery_decision, thresholds))
}

impl AppState {
    fn new_with_status(status: String) -> Self {
        AppState {
            prices: vec![],
            current_price: 0.0, block_threshold: 0.0, low_price_threshold: 0.0, spike_threshold: 0.0,
            compressor_decision: CompressorState::Allowed, soc_decision: 0,
            system_status: SystemStatus::default(),
            status_message: status,
        }
    }
}

/// Determines the baseline Minimum SOC based on the season.
fn get_baseline_soc(config: &Config, now: DateTime<Utc>) -> u8 {
    match now.month() {
        4..=9 => config.summer_min_soc, // April to September is Summer
        _ => config.winter_min_soc,    // October to March is Winter
    }
}
fn get_force_charge_soc(config: &Config, now: DateTime<Utc>) -> u8 {
    match now.month() { 4..=9 => config.summer_force_charge_soc, _ => config.winter_force_charge_soc }
}

/// Fetches the system status from the text file.
fn fetch_system_status(client: &Client, url: &str) -> Result<SystemStatus> {
    let response = client.get(url).send()
        .with_context(|| format!("Failed to connect to status URL: {}", url))?;
    
    if !response.status().is_success() {
         return Err(anyhow::anyhow!("Status URL returned {}", response.status()));
    }

    let text = response.text()?;
    let lines: Vec<&str> = text.lines().collect();

    if lines.len() < 5 {
        return Err(anyhow::anyhow!("Status file has insufficient lines"));
    }

    // Parse lines based on provided format
    // Line 0: SOC (int)
    // Line 1: PV (float)
    // Line 2: House (float)
    // Line 3: Grid (float)
    // Line 4: Battery Pwr (float)

    let soc = lines[0].trim().parse::<u8>().context("Failed to parse SOC")?;
    let pv = lines[1].trim().parse::<f64>().context("Failed to parse PV")?;
    let load = lines[2].trim().parse::<f64>().context("Failed to parse Load")?;
    let grid = lines[3].trim().parse::<f64>().context("Failed to parse Grid")?;
    let batt = lines[4].trim().parse::<f64>().context("Failed to parse Battery Power")?;

    Ok(SystemStatus {
        soc,
        pv_power: pv,
        load_power: load,
        grid_power: grid,
        battery_power: batt,
    })
}

fn set_minimum_soc(config: &Config, soc: u8) -> Result<()> {
    log::info!(
        "Set Minimum SOC to {}% on host {}",
        soc,
        config.ssh_host
    );
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
    while let Some(front) = prices.front() {
        if front.from < now - Duration::hours(1) {
            prices.pop_front();
        } else {
            break;
        }
    }

    // Check if we need to fetch data for today or tomorrow.
    let today = now.date_naive();
    let tomorrow = today.succ_opt().context("Failed to calculate tomorrow's date")?;

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
fn control_relay(
    config: &Config,
    desired_state: CompressorState,
    client: &Client,
) -> Result<()> {
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

/// Loads configuration from environment variables.
fn load_config() -> Result<Config> {
    Ok(Config {
        status_url: env::var("STATUS_URL").unwrap_or("http://192.168.178.11/status/soc.txt".to_string()),
        shelly_ip: env::var("SHELLY_IP").context("SHELLY_IP not set")?,
        relay_on_to_block: env::var("RELAY_ON_TO_BLOCK").unwrap_or("false".into()).parse()?,
        check_interval: time::Duration::from_secs(env::var("CHECK_INTERVAL_MINUTES").unwrap_or("5".into()).parse::<u64>()? * 60),
        block_price_percentile: env::var("BLOCK_PRICE_PERCENTILE").unwrap_or("75.0".into()).parse()?,
        max_continuous_block_minutes: env::var("MAX_CONTINUOUS_BLOCK_MINUTES").unwrap_or("120".into()).parse()?,
        min_rest_time_minutes: env::var("MIN_REST_TIME_MINUTES").unwrap_or("60".into()).parse()?,
        lookahead_hours: env::var("LOOKAHEAD_HOURS").unwrap_or("6".into()).parse()?,
        low_price_percentile: env::var("LOW_PRICE_PERCENTILE").unwrap_or("10.0".into()).parse()?,
        high_spike_percentile: env::var("HIGH_SPIKE_PERCENTILE").unwrap_or("95.0".into()).parse()?,
        min_spike_difference_cents: env::var("MIN_SPIKE_DIFFERENCE_CENTS").unwrap_or("10.0".into()).parse()?,
        enable_battery_control: env::var("ENABLE_BATTERY_CONTROL").unwrap_or("true".into()).parse()?,
        ssh_host: env::var("SSH_HOST").context("SSH_HOST not set")?,
        ssh_user: env::var("SSH_USER").context("SSH_USER not set")?,
        ssh_pass: env::var("SSH_PASS").context("SSH_PASS not set")?,
        winter_force_charge_soc: env::var("WINTER_FORCE_CHARGE_SOC").unwrap_or("60".into()).parse()?,
        summer_force_charge_soc: env::var("SUMMER_FORCE_CHARGE_SOC").unwrap_or("30".into()).parse()?,
        summer_min_soc: env::var("SUMMER_MIN_SOC").unwrap_or("10".into()).parse()?,
        winter_min_soc: env::var("WINTER_MIN_SOC").unwrap_or("20".into()).parse()?,
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

