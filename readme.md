# Energy Controller

<img width="1372" height="768" alt="image" src="https://github.com/user-attachments/assets/c083933c-abd0-46f6-aaed-edb9e9f91862" />

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

---
