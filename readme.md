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

### **Shelly Relay Configuration**

- **Local IP address of your Shelly Plus 1 relay:**  
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

- **Battery SOC status URL:**  
    ```env
    STATUS_URL="http://192.168.178.11/status/soc.txt"
    ```
    *URL to fetch the current battery state of charge (SOC) as a text file.*

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
