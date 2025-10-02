# Energy Controller

This program controls a Shelly Plus 1 relay to manage a heat pump based on electricity prices from the EPEX market index. 
It can also interface with a Victron battery system to optimize battery usage based on current and future price data.

## Features
- Fetches electricity prices from Spotty in JSON format.
- Controls a Shelly Plus 1 relay to block or allow heat pump operation based on electricity prices.
- Optional integration with a Victron battery system to manage battery state of charge (SOC) based on electricity prices.
- Configurable parameters for lookahead window, blocking slots, and battery SOC thresholds.

Note: This program does not manage heating and temperature. It simply suppresses heatpump operation during price spikes. This supression is configurable and there is a maximum delay so as to not let temperatures drop too much.

## Configuration

Create a `.env` file in the same directory as the executable with the following parameters:

---

### **Shelly Relay Configuration**

- **Local IP address of your Shelly Plus 1 relay:**  
    ```env
    SHELLY_IP="192.168.1.123"   # Example
    SHELLY_IP="YOUR_SHELLY_IP_HERE"
    ```

---

### **Heat Pump Logic Configuration**

- **Hours to look ahead when deciding whether to block the compressor:**  
    ```env
    LOOKAHEAD_HOURS=4
    ```

- **Number of 15-minute slots to block within the lookahead window:**  
    ```env
    BLOCK_SLOTS=4
    ```

- **How often the program checks the price and updates the state (in minutes):**  
    ```env
    CHECK_INTERVAL_MINUTES=5
    ```

- **Set to "true" if your relay needs to be ON to BLOCK the heatpump:**  
    ```env
    RELAY_ON_TO_BLOCK=false
    ```


The maximum duration for the heatpump being forced OFF is controlled by the interaction between two parameters:

    LOOKAHEAD_HOURS

    BLOCK_SLOTS

At any given moment, the program looks ahead for the duration of LOOKAHEAD_HOURS (default is 4 hours). Within that window, it identifies the BLOCK_SLOTS number of most expensive 15-minute intervals (default is 4 slots).

The compressor is only suppressed if the current time falls into one of those most expensive slots.

Using the default settings, this means that within any 4-hour sliding window, the compressor can be forced off for a maximum of 4 slots x 15 minutes/slot = 60 minutes.

This mechanism effectively prevents the heat pump from being turned off for too long. If a high-price period lasts for many hours, the "lookahead" window will eventually contain only high prices, and the logic will allow the compressor to run because the current price is no longer an extreme outlier within that window.

---

### **Battery SOC Control Configuration**

- **Enable or disable the battery SOC control feature:**  
    ```env
    ENABLE_BATTERY_CONTROL=true
    ```

---

### **SSH Credentials for the Victron / Battery System**

- **SSH host (example):**  
    ```env
    SSH_HOST="victron.local"
    SSH_HOST="192.168.1.200"
    SSH_HOST="victron"
    ```

- **SSH user:**  
    ```env
    SSH_USER="root"
    ```

- **SSH password:**  
    ```env
    SSH_PASS="password"
    ```

---

### **Battery Logic Parameters**

- **Price in cents/kWh below which to force battery charging:**  
    ```env
    LOW_PRICE_THRESHOLD=4.0
    ```

- **Price in cents/kWh considered a major spike (pre-charge if detected):**  
    ```env
    HIGH_PRICE_SPIKE_THRESHOLD=40.0
    ```

- **Target Minimum SOC (%) when force-charging:**  
    ```env
    FORCE_CHARGE_SOC=95
    ```

- **Baseline Minimum SOC (%) for Summer months (April–September):**  
    ```env
    SUMMER_MIN_SOC=10
    ```

- **Baseline Minimum SOC (%) for Winter months (October–March):**  
    ```env
    WINTER_MIN_SOC=20
    ```
