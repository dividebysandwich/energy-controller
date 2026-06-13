# Energy Controller

<img width="1211" height="902" alt="image" src="https://github.com/user-attachments/assets/f27b3f1c-4f7f-4b2c-aa4c-862a58dafb39" />

<img width="1234" height="711" alt="image_2026-03-04_16-37-53" src="https://github.com/user-attachments/assets/84413be6-ed54-4a9e-a232-ebd0a3f14ff3" />

This program controls the battery charge level of a Victron energy storage system to optimize grid energy usage, buying energy from the grid on price dips and preserving battery capacity for use during price spikes. The pricing info is fetched from EPEX / Spotty.

Optionally it can also control a Shelly power relay to manage a heat pump, temporarily preventing it to run during price spikes.

## Features
- Fetches electricity prices from Spotty in JSON format.
- Fetches temperature and solar irradiance forecast from open-meteo.com
- Prevents expensive operations of heat pumps during price spikes, using a Shelly Plus 1 relay wired to block the compressor startup
- Optional integration with a Victron battery system to manage battery state of charge (SOC) based on electricity prices.
- Intelligently detects price dips and starts force-charging the battery only at the beginning of a significant dip
- Different minimum battery SOC based on season, predicted heating power usage, and PV production
- Configurable parameters for lookahead window, blocking slots, and battery SOC thresholds.
- Percentile-based operation works regardless of current average price levels
- TUI based display of price graph, predicted PV yield, heating usage and current state

Note: This program does not manage heating and temperature. It simply suppresses heatpump operation during price spikes. This supression is configurable and there is a maximum delay so as to not let temperatures drop too much. You can still use this program even if you don't have a means to lock out your heating system during price spikes.

## Configuration

Create a `.env` file in the same directory as the executable with the following parameters:

---

### **Heatpump / Shelly Relay Configuration**

- **Enable heatpump control:**
    ```env
    ENABLE_HEATPUMP_CONTROL=true
    ```
    *Set to `false` to skip all relay control entirely. The price-based "would block" decision is still calculated and shown in the UI, but no commands are sent and `SHELLY_IP` is not required. Useful when you only want to use this program for battery / SOC management.*

- **Local IP address of your Shelly Plus 1 relay:** *(required when `ENABLE_HEATPUMP_CONTROL=true`)*
    ```env
    SHELLY_IP="192.168.1.123"   # Example
    SHELLY_IP="YOUR_SHELLY_IP_HERE"
    ```

- **Relay operation mode:**
    ```env
    RELAY_ON_TO_BLOCK=false
    ```
    *Whether the relay should be switched ON in order to inhibit heat pump operation. If set to false, relay will be switched ON to allow, and OFF to inhibit heat pump operation.*

---

### **General Settings**

- **Check interval (minutes):**  
    ```env
    CHECK_INTERVAL_MINUTES=5
    ```
    *How often the price/weather analysis and battery/heatpump decisions are re-evaluated.*

- **Live status poll interval (seconds):**
    ```env
    STATUS_POLL_SECONDS=10
    ```
    *How often the live system status (battery SOC, PV, load, grid, battery power) is refreshed and pushed to the web UI, independent of the slower `CHECK_INTERVAL_MINUTES` analysis loop. The web UI's own refresh rate is matched to this value automatically. **Mind your data source's rate limit:** the SolarEdge cloud API allows only ~300 requests/day, so a 10-second poll (~8,640/day) will exhaust the quota in under an hour — for fast live updates use a non-rate-limited local source (legacy `STATUS_URL` / inverter Modbus). For SolarEdge-only setups keep this around 300 (5 min).*

- **Enable price-based control:**
    ```env
    USE_PRICING=true
    ```
    *Set to `false` to skip fetching electricity prices entirely. The "Thresholds" panel disappears from the TUI and web UI, and price-based decisions are not made. Heatpump and battery control are automatically force-disabled in this mode (both depend on price data). Useful when you only want a status / monitoring display.*

- **Enable MCP server:**
    ```env
    ENABLE_MCP=true
    ```
    *Set to `false` to disable the built-in MCP (Model Context Protocol) server. When enabled (the default), it is served on the same port as the web UI and lets MCP-capable LLM/agent clients query live and historical energy data.*

---

### **MCP Server (energy data for LLM / agent clients)**

The program exposes a built-in [Model Context Protocol](https://modelcontextprotocol.io) server over the **HTTP + Server-Sent Events** transport, mounted on the same port as the web UI (`WEB_PORT`, default `8080`) under any configured `APP_CONTEXT_PATH`.

- **SSE stream endpoint (connect here):** `http://<host>:<port>/mcp/sse`
- **Message endpoint:** advertised automatically by the server as the first SSE `endpoint` event (`/mcp/messages?sessionId=...`).

Example client config:

```json
{
  "mcpServers": {
    "energy-controller": {
      "url": "http://192.168.1.50:8080/mcp/sse"
    }
  }
}
```

**Exposed tools** (all powers in kW, prices in euro-cents per kWh; names and result fields are self-describing):

- `get_current_household_energy_status` — latest live snapshot: battery state of charge (%), solar PV production, household consumption, grid power (±import/export), battery power (±discharge/charge).
- `get_household_energy_history_24h` — rolling 24-hour timestamped time series for one or all metrics (`battery_state_of_charge_percent`, `solar_pv_production_kw`, `household_consumption_load_kw`, `grid_power_kw`, `battery_power_kw`).
- `get_electricity_spot_price_forecast` — current price, the controller's percentile thresholds, and the upcoming hourly EPEX/Spotty prices.
- `get_solar_and_heating_power_forecast` — forecasted hourly solar PV production and estimated heating demand, plus expected remaining PV yield for today (kWh).
- `get_battery_and_heatpump_control_decisions` — the controller's current target SOC, heat-pump compressor allow/block decision, and status message.

---

### **Heat Pump Control**

- **Block price percentile:**  
    ```env
    BLOCK_PRICE_PERCENTILE=75.0
    ```
    *Blocks the heat pump if the current price is in the most expensive 25% of the day.*

- **Maximum continuous block time (minutes):**  
    ```env
    MAX_CONTINUOUS_BLOCK_MINUTES=120
    ```

- **Minimum rest time after block (minutes):**  
    ```env
    MIN_REST_TIME_MINUTES=60
    ```
    *After a block period ends, the heat pump must run for this duration before it can be blocked again.*

---

### **Battery SOC Control**

- **Lookahead window (hours):**  
    ```env
    LOOKAHEAD_HOURS=6
    ```
    *Number of hours to look ahead for price spikes.*

- **Low price percentile for charging:**  
    ```env
    LOW_PRICE_PERCENTILE=10.0
    ```
    *Force battery charging if the price is in the cheapest 10% of the day.*

- **High spike percentile:**  
    ```env
    HIGH_SPIKE_PERCENTILE=95.0
    ```
    *A price is considered a major spike if above this percentile; triggers pre-charging.*

- **Minimum price spike threshold:**
    ```env
    MIN_SPIKE_DIFFERENCE_CENTS=10.0
    ```
    *Only force-charge the battery if the detected price spike is significant enough to justify pre-charging the battery.*

- **Enable battery control:**  
    ```env
    ENABLE_BATTERY_CONTROL=true
    ```

- **Victron SSH connection:**  
    ```env
    SSH_HOST="victron"
    SSH_USER="root"
    SSH_PASS="password"
    ```

- **Force charge SOC (%):**  
    ```env
    FORCE_CHARGE_SOC=80
    ```

- **Minimum SOC for Summer (April–September):**  
    ```env
    SUMMER_MIN_SOC=10
    ```

- **Minimum SOC for Winter (October–March):**  
    ```env
    WINTER_MIN_SOC=20
    ```

### **Data Sources**

You can pull live system data (SOC, PV, load, grid, battery power) from one or more sources. Multiple sources can be enabled at the same time and their results are combined: SOC / consumption / battery power are taken from Huawei (or Legacy if Huawei is not enabled), while PV production and grid power are summed across all enabled sources.

- **Legacy status JSON URL (default source):**
    ```env
    USE_LEGACY_STATUS=true
    STATUS_URL="http://192.168.178.11/status/soc.txt"
    ```
    *URL returning a JSON array of `{time, BatterySOC, PV, Consumption, Grid, BatteryPower}` samples (last entry is current). This is the source supported in earlier versions and remains the default.*

- **Huawei FusionSolar Cloud API:**
    ```env
    USE_HUAWEI=false
    HUAWEI_API_URL="https://eu5.fusionsolar.huawei.com"
    HUAWEI_USERNAME="api_user"
    HUAWEI_SYSTEM_CODE="api_password"
    HUAWEI_STATION_CODE=""              # Optional; auto-discovered on first call if empty
    HUAWEI_INVERT_GRID_SIGN=false       # Set to true if grid_power sign comes out reversed
    ```
    *Requires a Northbound API account on FusionSolar (request from your installer). Provides SOC, battery power, PV production, grid power, and a derived household consumption (PV + battery_discharge + grid_import). Pick the API URL closest to your account region (eu5, intl, na5, etc.).*

- **SolarEdge Monitoring API:**
    ```env
    USE_SOLAREDGE=false
    SOLAREDGE_API_URL="https://monitoringapi.solaredge.com"
    SOLAREDGE_API_KEY="YOUR_KEY"
    SOLAREDGE_SITE_ID="123456"
    ```
    *Uses the `currentPowerFlow` endpoint. Provides PV production and grid power (no battery / consumption). When combined with Huawei, both inverters' production and grid contributions are summed.*

- **Fast live status source:**
    ```env
    USE_LIVE_STATUS=false
    LIVE_STATUS_URL="https://hoxdna.org/getEnergy"
    ```
    *Optional fast-updating live source. The URL must return a JSON object with the current scalar readings `{ "soc": 79, "pv": 9.3, "consumption": 5.4, "grid": 0.2, "batteryuse": 4.2 }` (powers in kW, SOC in percent), refreshed every few seconds. When enabled, these values override **only** the live current readings (SOC, PV, load, grid, battery power) shown in the UI/API/MCP — the **historical charts keep coming from the other configured source(s)** (legacy/Huawei/SolarEdge and their in-process history). Combine it with another source for histograms, or run it alone if you only need live values. If disabled, behavior is unchanged. Pair with `STATUS_POLL_SECONDS` to control how often it's polled. The endpoint is expected to report battery **discharge as a positive** `batteryuse`; this is inverted internally to match the rest of the app (positive = charging). Grid is passed through (+ = import).*

- **Site latitude (decimal degrees):**  
    ```env
    LATITUDE=50.0000
    ```
    *Geographic latitude of the installation site.*

- **Site longitude (decimal degrees):**  
    ```env
    LONGITUDE=13.0000
    ```
    *Geographic longitude of the installation site.*

- **Battery capacity (kWh):**  
    ```env
    BATTERY_SIZE_KWH=40.0
    ```
    *Total usable battery capacity in kilowatt-hours.*

- **Photovoltaic system size (kWp):**  
    ```env
    PV_SIZE_KWP=10.0
    ```
    *Installed solar panel capacity in kilowatt-peak.*

- **Base electrical load (kW):**  
    ```env
    BASE_LOAD_KW=0.5
    ```
    *Average background electrical consumption in kilowatts.*

- **Heating off temperature (°C):**  
    ```env
    HEATING_OFF_TEMP_C=15.0
    ```
    *Outdoor temperature above which heating is turned off.*

- **Heating energy use at 0°C (kWh/h):**  
    ```env
    HEATING_KWH_PER_H_AT_0C=1.5
    ```
    *Heating energy consumption per hour when outdoor temperature is 0°C.*

---
