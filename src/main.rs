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
};
use serde::Deserialize;
use ssh2::Session;
use std::{
    collections::VecDeque,
    env,
    io::{self, prelude::*},
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

/// Represents the desired state of the heatpump compressor.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum CompressorState {
    Allowed,
    Blocked,
}

/// Holds all configuration loaded from environment variables.
struct Config {
    // General
    check_interval: time::Duration,
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
    force_charge_soc: u8,
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
    status_message: String,
}


fn main() -> Result<()> {
    dotenv::dotenv().ok();
    let config = load_config()?;
    let (tx, rx) = mpsc::channel::<AppState>(); // Channel for logic thread to send data to UI thread
    
    thread::spawn(move || {
        // Build a reusable HTTP client that uses rustls for TLS.
        let client = Client::builder().use_rustls_tls().build().unwrap();
        // A double-ended queue to hold price data for today and tomorrow.
        let mut prices: VecDeque<PriceInfo> = VecDeque::new();
        // State variables to prevent unnecessary switching.
        let mut last_compressor_state = CompressorState::Allowed;
        let mut last_soc_set: u8 = 0;
        let mut block_start_time: Option<DateTime<Utc>> = None;
        let mut rest_until_time: Option<DateTime<Utc>> = None;

        // Initialize the relay to the 'Allowed' state on startup as a safe default.
        if let Err(e) = control_relay(&config, CompressorState::Allowed, &client) {
            // Log this error somewhere? For now, we ignore it.
             let _ = tx.send(AppState::new_with_status(format!("Startup relay error: {}", e)));
        }

        loop {
            let now = Utc::now();
            let result = run_price_analysis(&config, &mut prices, &client, now);
            
            match result {
                Ok((price_based_decision, desired_soc, thresholds)) => {
                    let mut final_compressor_decision = price_based_decision;
                    let mut status_message = String::from("OK");

                    // Reset block timer if price-based decision allows running.
                    if price_based_decision == CompressorState::Allowed {
                        block_start_time = None;
                    }

                    // Timing logic overrides
                    if let Some(rest_end) = rest_until_time {
                        if now < rest_end {
                            status_message = format!("In mandatory rest period until {}", rest_end.with_timezone(&Tz::CET).format("%H:%M"));
                            final_compressor_decision = CompressorState::Allowed;
                        } else {
                            rest_until_time = None;
                        }
                    }
                    // Check if the maximum block time has been exceeded.
                    if let Some(block_start) = block_start_time {
                         let block_duration = now.signed_duration_since(block_start);
                         if block_duration > Duration::minutes(config.max_continuous_block_minutes) {
                            status_message = format!("Max block time exceeded ({}m)", block_duration.num_minutes());
                            final_compressor_decision = CompressorState::Allowed;
                        }
                    }

                    // Update Timers on State Change
                    if final_compressor_decision != last_compressor_state {
                        match (last_compressor_state, final_compressor_decision) {
                            (CompressorState::Allowed, CompressorState::Blocked) => block_start_time = Some(now),
                            (CompressorState::Blocked, CompressorState::Allowed) => {
                                block_start_time = None;
                                rest_until_time = Some(now + Duration::minutes(config.min_rest_time_minutes));
                            },
                            _ => {}
                        }
                    }
                    
                    // Send commands if state changed
                    if final_compressor_decision != last_compressor_state {
                        if let Err(e) = control_relay(&config, final_compressor_decision, &client) {
                             let _ = tx.send(AppState::new_with_status(format!("Relay control error: {}", e)));
                        }
                    }
                    last_compressor_state = final_compressor_decision;

                    // Handle Battery SOC State
                    if config.enable_battery_control && desired_soc != last_soc_set {
                        if let Err(e) = set_minimum_soc(&config, desired_soc) {
                             let _ = tx.send(AppState::new_with_status(format!("SSH error: {}", e)));
                        }
                        last_soc_set = desired_soc;
                    }

                    // Send complete state to UI thread
                    let app_state = AppState {
                        prices: prices.iter().cloned().collect(),
                        current_price: thresholds.current_price,
                        block_threshold: thresholds.block_threshold,
                        low_price_threshold: thresholds.low_price_threshold,
                        spike_threshold: thresholds.spike_threshold,
                        compressor_decision: final_compressor_decision,
                        soc_decision: desired_soc,
                        status_message,
                    };
                    if tx.send(app_state).is_err() { break; } // Stop if UI thread has panicked
                },
                Err(e) => {
                    // On error, send status and ensure failsafe state
                    if last_compressor_state == CompressorState::Blocked {
                        if control_relay(&config, CompressorState::Allowed, &client).is_ok() {
                            last_compressor_state = CompressorState::Allowed;
                            block_start_time = None;
                        }
                    }
                    if tx.send(AppState::new_with_status(format!("Error: {}", e))).is_err() { break; }
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
        // Update state if new data is available
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
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    Ok(())
}


// UI Rendering

fn ui(f: &mut Frame, app_state: Option<&AppState>) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(70), Constraint::Percentage(30)].as_ref())
        .split(f.area());
    
    let top_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)].as_ref())
        .split(chunks[1]);

    // --- Chart ---
    let chart_block = Block::default().title("Price Data (Today)").borders(Borders::ALL);
    f.render_widget(chart_block, chunks[0]);

    if let Some(state) = app_state {
        let (x_bounds, y_bounds, price_data, line_data) = prepare_chart_data(state);

        let datasets = vec![
            Dataset::default()
                .name("Price")
                .marker(symbols::Marker::Dot)
                .graph_type(GraphType::Line)
                .style(Style::default().fg(Color::Cyan))
                .data(&price_data),
            Dataset::default()
                .name("Now")
                .marker(symbols::Marker::Braille)
                .graph_type(GraphType::Line)
                .style(Style::default().fg(Color::Yellow))
                .data(&line_data),
        ];

        let chart = Chart::new(datasets)
            .x_axis(
                Axis::default()
                    .title("Time (CET)")
                    .style(Style::default().fg(Color::Gray))
                    .bounds(x_bounds)
                    .labels(
                        x_bounds
                            .iter()
                            .map(|&t| {
                                Span::from(
                                    DateTime::from_timestamp(t as i64, 0)
                                        .unwrap()
                                        .with_timezone(&Tz::CET)
                                        .format("%H:%M")
                                        .to_string(),
                                )
                            })
                            .collect::<Vec<Span>>()
                    ),
            )
            .y_axis(
                Axis::default()
                    .title("Price (cents/kWh)")
                    .style(Style::default().fg(Color::Gray))
                    .bounds(y_bounds)
                    .labels(
                        y_bounds
                            .iter()
                            .map(|&p| Span::from(format!("{:.1}", p)))
                            .collect::<Vec<Span>>(),
                    ),
            );

        f.render_widget(chart, chunks[0]);
    }

    // Percentile Info
    let percentile_block = Block::default().title("Current Thresholds").borders(Borders::ALL);
    let mut percentile_text = vec![];
    if let Some(state) = app_state {
         percentile_text.push(Line::from(vec![Span::raw("Current Price: "), Span::styled(format!("{:.2}c", state.current_price), Style::default().fg(Color::White).add_modifier(Modifier::BOLD))]));
         percentile_text.push(Line::from(vec![Span::raw("Low Price < "), Span::styled(format!("{:.2}c", state.low_price_threshold), Style::default().fg(Color::Green))]));
         percentile_text.push(Line::from(vec![Span::raw("Block Price > "), Span::styled(format!("{:.2}c", state.block_threshold), Style::default().fg(Color::Rgb(255, 165, 0)))]));
         percentile_text.push(Line::from(vec![Span::raw("Spike Price > "), Span::styled(format!("{:.2}c", state.spike_threshold), Style::default().fg(Color::Red))]));
    }
    let percentile_paragraph = Paragraph::new(percentile_text).block(percentile_block);
    f.render_widget(percentile_paragraph, top_chunks[0]);

    // Decision Info
    let decision_block = Block::default().title("Decisions & Status").borders(Borders::ALL);
    let mut decision_text = vec![];
     if let Some(state) = app_state {
        let (comp_text, comp_style) = match state.compressor_decision {
            CompressorState::Allowed => ("Allowed", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
            CompressorState::Blocked => ("BLOCKED", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
        };
        decision_text.push(Line::from(vec![Span::raw("Compressor: "), Span::styled(comp_text, comp_style)]));
        decision_text.push(Line::from(vec![Span::raw("Battery Min SOC: "), Span::styled(format!("{}%", state.soc_decision), Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))]));
        decision_text.push(Line::from(vec![Span::raw(" ")])); // Spacer
        decision_text.push(Line::from(vec![Span::raw("Status: "), Span::styled(&state.status_message, Style::default().fg(Color::Yellow))]));

    }
    let decision_paragraph = Paragraph::new(decision_text).block(decision_block);
    f.render_widget(decision_paragraph, top_chunks[1]);
}

fn prepare_chart_data(state: &AppState) -> ([f64; 2], [f64; 2], Vec<(f64, f64)>, Vec<(f64, f64)>) {
    let now = Utc::now();
    let today = now.with_timezone(&Tz::CET).date_naive();
    
    let prices_today: Vec<_> = state.prices.iter().filter(|p| p.from.with_timezone(&Tz::CET).date_naive() == today).collect();

    if prices_today.is_empty() { return ([0.0, 0.0], [0.0, 0.0], vec![], vec![]); }

    let price_data: Vec<(f64, f64)> = prices_today.iter().map(|p| (p.from.timestamp() as f64, p.price)).collect();
    
    let min_price = prices_today.iter().map(|p| p.price).fold(f64::INFINITY, f64::min);
    let max_price = prices_today.iter().map(|p| p.price).fold(f64::NEG_INFINITY, f64::max);
    let y_bounds = [min_price * 0.95, max_price * 1.05];

    let x_min = prices_today.first().unwrap().from.timestamp() as f64;
    let x_max = prices_today.last().unwrap().from.timestamp() as f64;
    let x_bounds = [x_min, x_max];

    let now_ts = now.timestamp() as f64;
    let line_data = vec![(now_ts, y_bounds[0]), (now_ts, y_bounds[1])];

    (x_bounds, y_bounds, price_data, line_data)
}


// --- Logic (largely unchanged, but split from main loop) ---

struct PriceThresholds { current_price: f64, block_threshold: f64, low_price_threshold: f64, spike_threshold: f64, }

fn run_price_analysis(
    config: &Config, prices: &mut VecDeque<PriceInfo>, client: &Client, now: DateTime<Utc>
) -> Result<(CompressorState, u8, PriceThresholds)> {
    update_price_data(prices, now, client).context("Failed to update price data")?;

    let current_price_info = prices.iter().find(|p| now >= p.from && now < p.from + Duration::minutes(15)).context("Could not find current price information")?;
    let mut todays_prices_values: Vec<f64> = prices.iter().filter(|p| p.from.date_naive() == now.date_naive()).map(|p| p.price).collect();
    if todays_prices_values.len() < 4 { return Err(anyhow::anyhow!("Not enough price data for today")); }

    let block_threshold = percentile(&mut todays_prices_values, config.block_price_percentile / 100.0);
    let low_price_threshold = percentile(&mut todays_prices_values, config.low_price_percentile / 100.0);
    let spike_threshold = percentile(&mut todays_prices_values, config.high_spike_percentile / 100.0);
    
    let thresholds = PriceThresholds { current_price: current_price_info.price, block_threshold, low_price_threshold, spike_threshold, };

    let price_based_decision = if current_price_info.price > block_threshold { CompressorState::Blocked } else { CompressorState::Allowed };

    let baseline_soc = get_baseline_soc(config, now);
    let mut soc_decision = baseline_soc;

    if config.enable_battery_control {
        let lookahead_end_time = now + Duration::hours(config.lookahead_hours);
        let max_future_price_info = prices.iter().filter(|p| p.from >= now && p.from < lookahead_end_time).max_by(|a, b| a.price.partial_cmp(&b.price).unwrap());
        let mut upcoming_spike_triggers_charge = false;
        if let Some(max_price) = max_future_price_info {
            let price_diff = max_price.price - current_price_info.price;
            if max_price.price > spike_threshold && price_diff > config.min_spike_difference_cents { upcoming_spike_triggers_charge = true; }
        }
        if current_price_info.price < low_price_threshold { soc_decision = config.force_charge_soc; } 
        else if upcoming_spike_triggers_charge { soc_decision = config.force_charge_soc; }
    }
    Ok((price_based_decision, soc_decision, thresholds))
}

impl AppState {
    fn new_with_status(status: String) -> Self {
        AppState {
            prices: vec![],
            current_price: 0.0, block_threshold: 0.0, low_price_threshold: 0.0, spike_threshold: 0.0,
            compressor_decision: CompressorState::Allowed, soc_decision: 0,
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

/// Connects to the Victron system via SSH and sets the Minimum SOC.
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
        force_charge_soc: env::var("FORCE_CHARGE_SOC").unwrap_or("80".into()).parse()?,
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

