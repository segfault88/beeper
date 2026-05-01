# Beeper

A BLE demo built during a hackathon. An ESP32-C6 periodically advertises over BLE and blinks an external LED string when triggered. A Flutter Android app scans for the device, shows approximate distance, and sends the trigger.

## Architecture

```
┌─────────────────────────┐        BLE         ┌──────────────────────┐
│  ESP32-C6 (firmware/)   │ ◄────────────────► │  Android app (app/)  │
│  Embassy + trouble-host │                    │  Flutter             │
└─────────────────────────┘                    └──────────────────────┘
```

### BLE Protocol

| Item              | Value                                  |
|-------------------|----------------------------------------|
| Device name       | `Beeper`                               |
| Service UUID      | `0000BEEF-0000-1000-8000-00805F9B34FB` |
| Trigger char UUID | `0000BEE0-0000-1000-8000-00805F9B34FB` |
| Trigger action    | Write any byte → LED blinks for 3 s   |

---

## Firmware (`firmware/`)

ESP32-C6 running the [Embassy](https://embassy.dev) async Rust framework.

**What it does:**
- Advertises as `Beeper` continuously over BLE
- Runs a GATT server; writing to the trigger characteristic blinks GPIO7 for 3 seconds (12 × 250 ms toggles via MOSFET → LED string)

**Stack:**
- `esp-hal 1.1` + `esp-radio 0.18` + `esp-rtos 0.3`
- `trouble-host 0.6` for GATT

### Prerequisites

```sh
# RISC-V target (one-time)
rustup target add riscv32imac-unknown-none-elf

# Flash tool (one-time)
cargo install espflash
```

### Build & Flash

```sh
cd firmware
cargo build --release
espflash flash --monitor target/riscv32imac-unknown-none-elf/release/beeper-firmware
```

### Hardware

Connect GPIO7 to the gate of a MOSFET; the MOSFET switches the LED string. Adjust the pin in `src/main.rs` if your wiring differs.

---

## Android App (`app/`)

Flutter app (Android only).

**What it does:**
- Scans for BLE devices advertising the Beeper service UUID
- Shows each device with a raw RSSI-based distance estimate
- Connects to a device, polls RSSI every 2 s for a live distance readout
- "Blink LED" button writes to the trigger characteristic

**Stack:** Flutter 3.x · `flutter_blue_plus 2.2` · `permission_handler 11.4`

### Prerequisites

- Flutter SDK (tested on 3.41)
- Android device with BLE (API 21+)

### Run

```sh
cd app
flutter pub get
flutter run
```

Accept the Bluetooth and location permission prompts on first launch.

### Distance estimation

Raw RSSI converted to metres using the log-distance path loss model:

```
d = 10 ^ ((txPower - rssi) / (10 × n))
```

`txPower = -59 dBm` (measured RSSI at 1 m), `n = 2.5` (indoor path loss exponent). A Kalman filter could improve accuracy — left as a future improvement.
