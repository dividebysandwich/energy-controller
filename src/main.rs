use anyhow::{Context, Result};
use chrono::{Datelike, DateTime, Duration, NaiveDate, Utc};
use reqwest::blocking::Client;
use serde::Deserialize;
use ssh2::Session;
use std::collections::VecDeque;
use std::env;
use std::io::Read;
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
    let mut last_soc_set: u8 = 0;
    let mut block_start_time: Option<DateTime<Utc>> = None;
    let mut rest_until_time: Option<DateTime<Utc>> = None;

    // Initialize the relay to the 'Allowed' state on startup as a safe default.
    if let Err(e) = control_relay(&config, CompressorState::Allowed, &client) {
        log::error!("Failed to initialize relay state on startup: {}", e);
    }

    // The main application loop.
    loop {
        let now = Utc::now();
        match run_price_analysis(&config, &mut prices, &client, now) {
            Ok((price_based_decision, desired_soc)) => {
                let mut final_compressor_decision = price_based_decision;

                // Stateful Timing Logic
                // Check if we are in a mandatory rest period.
                if let Some(rest_end) = rest_until_time {
                    if now < rest_end {
                        log::info!("In mandatory rest period until {}. Forcing ALLOW.", rest_end.to_rfc3339());
                        final_compressor_decision = CompressorState::Allowed;
                    } else {
                        log::info!("Rest period ended at {}.", rest_end.to_rfc3339());
                        rest_until_time = None;
                    }
                }

                // Check if the maximum block time has been exceeded.
                if let Some(block_start) = block_start_time {
                    let block_duration = now.signed_duration_since(block_start);
                    if block_duration > Duration::minutes(config.max_continuous_block_minutes) {
                        log::warn!(
                            "Max block time of {}m exceeded (blocked for {}m). Forcing ALLOW.",
                            config.max_continuous_block_minutes,
                            block_duration.num_minutes()
                        );
                        final_compressor_decision = CompressorState::Allowed;
                    }
                }

                // Update Timers on State Change
                if final_compressor_decision != last_compressor_state {
                    match (last_compressor_state, final_compressor_decision) {
                        (CompressorState::Allowed, CompressorState::Blocked) => {
                            log::info!("State changing: ALLOWED -> BLOCKED. Starting block timer.");
                            block_start_time = Some(now);
                        }
                        (CompressorState::Blocked, CompressorState::Allowed) => {
                            log::info!("State changing: BLOCKED -> ALLOWED. Starting rest timer.");
                            block_start_time = None;
                            rest_until_time = Some(now + Duration::minutes(config.min_rest_time_minutes));
                        }
                        _ => {} // Should not happen
                    }
                }

                // Send Relay Command
                if final_compressor_decision != last_compressor_state {
                    log::info!("Final decision -> {:?}", final_compressor_decision);
                    match control_relay(&config, final_compressor_decision, &client) {
                        Ok(_) => {
                            log::info!("Successfully changed relay state.");
                            last_compressor_state = final_compressor_decision;
                        }
                        Err(e) => log::error!("Failed to update relay state: {}", e),
                    }
                }

                // Handle Battery SOC State
                if config.enable_battery_control && desired_soc != last_soc_set {
                    log::info!("Battery SOC change -> {}%", desired_soc);
                    match set_minimum_soc(&config, desired_soc) {
                        Ok(_) => {
                            log::info!("Successfully set new Minimum SOC.");
                            last_soc_set = desired_soc;
                        }
                        Err(e) => log::error!("Failed to set Minimum SOC via SSH: {:#}", e),
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
                        &client,
                    ) {
                        log::error!("Failed to set failsafe 'Allowed' state: {}", e_failsafe);
                    } else {
                        last_compressor_state = CompressorState::Allowed;
                        block_start_time = None; // Reset timer on failsafe
                        log::warn!("Set relay to failsafe 'Allowed' state due to error.");
                    }
                }
            }
        }
        log::debug!("Sleeping for {:?}...", config.check_interval);
        sleep(config.check_interval);
    }
}

/// Executes the price analysis part of the logic.
fn run_price_analysis(
    config: &Config,
    prices: &mut VecDeque<PriceInfo>,
    client: &Client,
    now: DateTime<Utc>,
) -> Result<(CompressorState, u8)> {
    update_price_data(prices, now, client).context("Failed to update price data")?;

    let current_price_info = prices
        .iter()
        .find(|p| now >= p.from && now < p.from + Duration::minutes(15))
        .context("Could not find current price information")?;

    log::info!("Current price: {:.2} cents/kWh.", current_price_info.price);

    let mut todays_prices_values: Vec<f64> = prices
        .iter()
        .filter(|p| p.from.date_naive() == now.date_naive())
        .map(|p| p.price)
        .collect();

    if todays_prices_values.len() < 4 {
        return Err(anyhow::anyhow!("Not enough price data for today to make a relative decision."));
    }

    let block_threshold = percentile(&mut todays_prices_values, config.block_price_percentile / 100.0);
    let low_price_threshold = percentile(&mut todays_prices_values, config.low_price_percentile / 100.0);
    let spike_threshold = percentile(&mut todays_prices_values, config.high_spike_percentile / 100.0);
    
    log::info!(
        "Today's relative price thresholds: Low < {:.2}, Block > {:.2}, Spike > {:.2}",
        low_price_threshold, block_threshold, spike_threshold
    );

    let price_based_decision = if current_price_info.price > block_threshold {
        log::info!("Heatpump decision: BLOCK. Current price {:.2} > threshold {:.2}.", current_price_info.price, block_threshold);
        CompressorState::Blocked
    } else {
        log::info!("Heatpump decision: ALLOW. Current price {:.2} <= threshold {:.2}.", current_price_info.price, block_threshold);
        CompressorState::Allowed
    };

    let baseline_soc = get_baseline_soc(config, now);
    let mut soc_decision = baseline_soc;

    if config.enable_battery_control {
        // Find the highest price in the lookahead window to check for a spike.
        let lookahead_end_time = now + Duration::hours(config.lookahead_hours);
        let max_future_price_info = prices
            .iter()
            .filter(|p| p.from >= now && p.from < lookahead_end_time)
            .max_by(|a, b| a.price.partial_cmp(&b.price).unwrap_or(std::cmp::Ordering::Equal));

        let mut upcoming_spike_triggers_charge = false;
        if let Some(max_price) = max_future_price_info {
            let price_diff = max_price.price - current_price_info.price;
            // A spike triggers a charge if it's above the percentile AND the difference is large enough.
            if max_price.price > spike_threshold {
                log::info!(
                    "Detected upcoming spike of {:.2} at {}.",
                    max_price.price,
                    max_price.from.to_rfc3339()
                );
                if price_diff > config.min_spike_difference_cents {
                    log::warn!(
                        "Battery Decision: Force charge due to upcoming spike of {:.2} detected (diff: {:.2} > {:.2}).",
                        max_price.price, price_diff, config.min_spike_difference_cents
                    );
                    upcoming_spike_triggers_charge = true;
                }
            }
        }

        if current_price_info.price < low_price_threshold {
            log::warn!("Battery Decision: Force charge. Price {:.2} is below the {:.0}th percentile threshold of {:.2}.", current_price_info.price, config.low_price_percentile, low_price_threshold);
            soc_decision = config.force_charge_soc;
        } else if upcoming_spike_triggers_charge {
            // Already logged the reason above.
            soc_decision = config.force_charge_soc;
        } else {
            log::info!("Battery Decision: Use baseline SOC of {}%.", baseline_soc);
        }
    }

    Ok((price_based_decision, soc_decision))
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

/// Provides a summary of the loaded configuration for logging.
fn config_summary(config: &Config) -> String {
    format!(
        "Block Percentile: {:.1}%, Max Block: {}m, Min Rest: {}m, Battery Control: {}",
        config.block_price_percentile, config.max_continuous_block_minutes, config.min_rest_time_minutes, config.enable_battery_control
    )
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

