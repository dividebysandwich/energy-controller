use anyhow::{Context, Result};
use chrono::{Datelike, DateTime, Duration, NaiveDate, Utc};
use reqwest::blocking::Client;
use serde::Deserialize;
use ssh2::Session;
use std::collections::VecDeque;
use std::env;
use std::io::prelude::*;
use std::net::TcpStream;
use std::thread::sleep;
use std::time;

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
    // Relay settings
    shelly_ip: String,
    relay_on_to_block: bool,
    // Heatpump logic
    lookahead_hours: i64,
    block_slots: usize,
    check_interval: time::Duration,
    // Battery control settings
    enable_battery_control: bool,
    ssh_host: String,
    ssh_user: String,
    ssh_pass: String,
    low_price_threshold: f64,
    high_price_spike_threshold: f64,
    force_charge_soc: u8,
    summer_min_soc: u8,
    winter_min_soc: u8,
}

/// Main entry point for the application.
fn main() -> Result<()> {
    // Initialize logging and load configuration
    env_logger::init();
    dotenv::dotenv().ok();
    let config = load_config()?;

    log::info!("Starting heatpump controller...");
    log::info!("Configuration loaded: {}", config_summary(&config));

    // Build a reusable HTTP client that uses rustls for TLS.
    let client = Client::builder()
        .use_rustls_tls()
        .build()
        .context("Failed to build HTTP client")?;

    // A double-ended queue to hold price data for today and tomorrow.
    let mut prices: VecDeque<PriceInfo> = VecDeque::new();
    // State variables to prevent unnecessary switching.
    let mut last_compressor_state = CompressorState::Allowed;
    let mut last_soc_set: u8 = 0; // Use 0 to ensure the first real value is always set

    // Initialize the relay to the 'Allowed' state on startup as a safe default.
    if let Err(e) = control_relay(&config, CompressorState::Allowed, None, &client) {
        log::error!("Failed to initialize relay state on startup: {}", e);
    }

    // The main application loop.
    loop {
        // Run a single logic cycle to get desired states for compressor and battery SOC.
        match run_logic_cycle(&config, &mut prices, &client) {
            Ok((desired_compressor_state, desired_soc)) => {
                // --- Handle Compressor State ---
                // Only send a command if the desired state has changed.
                if desired_compressor_state != last_compressor_state {
                    log::info!(
                        "Compressor state change detected! Desired: {:?}",
                        desired_compressor_state
                    );
                    match control_relay(
                        &config,
                        desired_compressor_state,
                        Some(last_compressor_state),
                        &client,
                    ) {
                        Ok(_) => {
                            log::info!("Successfully changed relay state.");
                            last_compressor_state = desired_compressor_state;
                        }
                        Err(e) => log::error!("Failed to update relay state: {}", e),
                        // We don't update last_state, so it will retry on the next cycle.
                    }
                } else {
                    log::debug!(
                        "No compressor state change needed. Current state is {:?}",
                        last_compressor_state
                    );
                }

                // --- Handle Battery SOC State ---
                if config.enable_battery_control && desired_soc != last_soc_set {
                    log::info!("Battery SOC change detected! Desired Min SOC: {}%", desired_soc);
                    match set_minimum_soc(&config, desired_soc) {
                        Ok(_) => {
                            log::info!("Successfully set new Minimum SOC.");
                            last_soc_set = desired_soc;
                        }
                        Err(e) => log::error!("Failed to set Minimum SOC via SSH: {:#}", e),
                        // We don't update last_soc_set, so it will retry on the next cycle.
                    }
                }
            }
            Err(e) => {
               // Using {:#} with anyhow::Error prints the full error chain for better debugging.
               log::error!(
                    "An error occurred in the logic cycle: {:#}. Will retry.",
                    e
                );
                // Failsafe: allow compressor to run if logic fails.
                if last_compressor_state == CompressorState::Blocked {
                    if let Err(e_failsafe) = control_relay(
                        &config,
                        CompressorState::Allowed,
                        Some(last_compressor_state),
                        &client,
                    ) {
                        log::error!("Failed to set failsafe 'Allowed' state: {}", e_failsafe);
                    } else {
                        last_compressor_state = CompressorState::Allowed;
                        log::warn!("Set relay to failsafe 'Allowed' state due to error.");
                    }
                }
            }
        }

        log::debug!("Sleeping for {:?}...", config.check_interval);
        sleep(config.check_interval);
    }
}

/// Executes one iteration of the control logic for both compressor and battery.
/// Returns a tuple: (desired_compressor_state, desired_minimum_soc)
fn run_logic_cycle(
    config: &Config,
    prices: &mut VecDeque<PriceInfo>,
    client: &Client,
) -> Result<(CompressorState, u8)> {
    let now = Utc::now();

    // Ensure we have price data for today and tomorrow.
    update_price_data(prices, now, client).context("Failed to update price data")?;

    // Find the current 15-minute price slot.
    let current_price_info = prices
        .iter()
        .find(|p| now >= p.from && now < p.from + Duration::minutes(15))
        .context("Could not find current price information. Data might be stale or incomplete.")?;

    log::info!(
        "Current time: {}. Current price: {:.2} cents/kWh.",
        now.to_rfc3339(),
        current_price_info.price
    );

    // Get all price slots within the configured lookahead window.
    let lookahead_end_time = now + Duration::hours(config.lookahead_hours);
    let future_prices_full: Vec<&PriceInfo> = prices
        .iter()
        .filter(|p| p.from >= now && p.from < lookahead_end_time)
        .collect();

    if future_prices_full.is_empty() {
        log::warn!("Lookahead window is empty. Allowing compressor and using baseline SOC.");
        let baseline_soc = get_baseline_soc(config, now);
        return Ok((CompressorState::Allowed, baseline_soc));
    }

    let mut future_prices_values: Vec<f64> =
        future_prices_full.iter().map(|p| p.price).collect();

    // Sort the prices in the lookahead window from most expensive to least expensive.
    future_prices_values.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));


    // Find the price that acts as our threshold. If the current price is higher than or equal to this, we should block.
    // For example, if BLOCK_SLOTS is 4, this will be the 4th most expensive price.
    let threshold_price = future_prices_values
        .get(config.block_slots.saturating_sub(1))
        .unwrap_or(&future_prices_values[future_prices_values.len() - 1]);

    // The core decision: block if the current price is among the most expensive in the near future.
    let compressor_decision = if current_price_info.price >= *threshold_price {
        log::warn!(
            "Decision: BLOCK. Current price {:.2} is at or above threshold {:.2}.",
            current_price_info.price,
            threshold_price
        );
        CompressorState::Blocked
    } else {
        log::info!(
            "Decision: ALLOW. Current price {:.2} is below threshold {:.2}.",
            current_price_info.price,
            threshold_price
        );
        CompressorState::Allowed
    };

    // --- Battery SOC Logic ---
    let baseline_soc = get_baseline_soc(config, now);
    let mut soc_decision = baseline_soc;

    if config.enable_battery_control {
        let upcoming_spike = future_prices_full
            .iter()
            .any(|p| p.price > config.high_price_spike_threshold);

        if current_price_info.price < config.low_price_threshold {
            log::warn!(
                "Battery Decision: Force charge. Price {:.2} is below threshold {}.",
                current_price_info.price,
                config.low_price_threshold
            );
            soc_decision = config.force_charge_soc;
        } else if upcoming_spike {
            log::warn!(
                "Battery Decision: Force charge. Upcoming spike > {:.2} detected.",
                config.high_price_spike_threshold
            );
            soc_decision = config.force_charge_soc;
        } else {
            log::info!(
                "Battery Decision: Use baseline SOC of {}%.",
                baseline_soc
            );
        }
    }

    Ok((compressor_decision, soc_decision))
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
        "Attempting to set Minimum SOC to {}% on host {}",
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

    log::info!("SSH command successful (exit code {}).", exit_status);
    Ok(())
}

// --- Functions from previous steps (unchanged or with minor updates) ---

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
        log::info!("Fetching price data for today ({})", today);
        prices.extend(
            fetch_prices_for_day(today, client)
                .with_context(|| format!("Failed to fetch prices for {}", today))?,
        );
    }
    if !prices.iter().any(|p| p.from.date_naive() == tomorrow) {
        log::info!("Fetching price data for tomorrow ({})", tomorrow);
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
    _last_state: Option<CompressorState>,
    client: &Client,
) -> Result<()> {
    let action = match (desired_state, config.relay_on_to_block) {
        (CompressorState::Blocked, true) => "on",
        (CompressorState::Allowed, true) => "off",
        (CompressorState::Blocked, false) => "off",
        (CompressorState::Allowed, false) => "on",
    };
    let url = format!("http://{}/relay/0?turn={}", config.shelly_ip, action);
    log::info!("Sending request to Shelly: {}", url);
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
        relay_on_to_block: env::var("RELAY_ON_TO_BLOCK").unwrap_or("true".into()).parse()?,
        lookahead_hours: env::var("LOOKAHEAD_HOURS").unwrap_or("4".into()).parse()?,
        block_slots: env::var("BLOCK_SLOTS").unwrap_or("4".into()).parse()?,
        check_interval: time::Duration::from_secs(
            env::var("CHECK_INTERVAL_MINUTES").unwrap_or("5".into()).parse::<u64>()? * 60,
        ),
        enable_battery_control: env::var("ENABLE_BATTERY_CONTROL").unwrap_or("true".into()).parse()?,
        ssh_host: env::var("SSH_HOST").context("SSH_HOST not set")?,
        ssh_user: env::var("SSH_USER").context("SSH_USER not set")?,
        ssh_pass: env::var("SSH_PASS").context("SSH_PASS not set")?,
        low_price_threshold: env::var("LOW_PRICE_THRESHOLD").unwrap_or("4.0".into()).parse()?,
        high_price_spike_threshold: env::var("HIGH_PRICE_SPIKE_THRESHOLD").unwrap_or("40.0".into()).parse()?,
        force_charge_soc: env::var("FORCE_CHARGE_SOC").unwrap_or("95".into()).parse()?,
        summer_min_soc: env::var("SUMMER_MIN_SOC").unwrap_or("10".into()).parse()?,
        winter_min_soc: env::var("WINTER_MIN_SOC").unwrap_or("20".into()).parse()?,
    })
}

/// Provides a simple string summary of the configuration for logging.
fn config_summary(config: &Config) -> String {
    format!(
        "Relay IP: {}, Lookahead: {}h, Block Slots: {}, Battery Control Enabled: {}",
        config.shelly_ip,
        config.lookahead_hours,
        config.block_slots,
        config.enable_battery_control
    )
}

