# nomopractic — Architecture

## System Context

```
┌──────────────────────────────────────────────────────────────────────┐
│                    Raspberry Pi Zero 2W (aarch64)                    │
│                                                                      │
│  ┌──────────────────────────────────┐                                │
│  │     nomothetic (Python)          │                                │
│  │  ┌─────────┐  ┌──────────────┐  │                                │
│  │  │   API   │  │  Telemetry   │  │                                │
│  │  │ :8443   │  │  (MQTT)      │  │                                │
│  │  └────┬────┘  └──────────────┘  │                                │
│  │       │                          │                                │
│  │  ┌────┴────┐  ┌──────────────┐  │                                │
│  │  │  Camera │  │  Streaming   │  │                                │
│  │  │         │  │  :8000       │  │                                │
│  │  └─────────┘  └──────────────┘  │                                │
│  │       │                          │                                │
│  │  ┌────┴────┐                     │                                │
│  │  │HatClient│ ← IPC client (no HW logic)                         │
│  │  └────┬────┘                     │                                │
│  └───────┼──────────────────────────┘                                │
│          │ Unix socket (NDJSON)                                      │
│          │ /run/nomopractic/nomopractic.sock                         │
│  ┌───────┴──────────────────────────┐                                │
│  │   nomopractic (Rust daemon)      │  ← THIS REPO                  │
│  │                                   │                                │
│  │  ┌─────────┐  ┌──────────────┐   │                                │
│  │  │   IPC   │  │   Config     │   │                                │
│  │  │ listener│  │  (TOML+env)  │   │                                │
│  │  └────┬────┘  └──────────────┘   │                                │
│  │       │                           │                                │
│  │  ┌────┴────────────────────────┐  │                                │
│  │  │     HAT Driver Layer        │  │                                │
│  │  │  ┌─────┐ ┌─────┐ ┌──────┐  │  │                                │
│  │  │  │Servo│ │ ADC │ │ GPIO │  │  │                                │
│  │  │  └──┬──┘ └──┬──┘ └──┬───┘  │  │                                │
│  │  │     │       │       │       │  │                                │
│  │  │  ┌──┴───────┴───────┴────┐  │  │                                │
│  │  │  │   PWM    │    I2C     │  │  │                                │
│  │  │  └──────────┴────────────┘  │  │                                │
│  │  └─────────────────────────────┘  │                                │
│  └───────────────────────────────────┘                                │
│          │                                                            │
│  ════════╪════════════════════════════════════════  Hardware bus      │
│          │                                                            │
│  ┌───────┴──────────────────────────┐                                │
│  │   SunFounder Robot HAT V4       │                                │
│  │   I2C bus 1, address 0x14       │                                │
│  │                                   │                                │
│  │   PWM channels 0–11 (servos)     │                                │
│  │   ADC A4 (battery voltage)       │                                │
│  │   GPIO: D4, D5, MCURST, SW, LED │                                │
│  └───────────────────────────────────┘                                │
└──────────────────────────────────────────────────────────────────────┘
```

## Module Dependency Graph

```
main.rs
  ├── config.rs          (CLI + TOML + env)
  ├── ipc/
  │   ├── mod.rs          (Unix socket listener)
  │   ├── schema.rs       (NDJSON request/response types)
  │   ├── handler.rs      (method dispatch → HAT drivers)
  │   └── params.rs       (typed IPC parameter extraction helpers)
  ├── hat/
  │   ├── mod.rs          (HAT abstraction, shared state)
  │   ├── i2c.rs          (rppal I2C read/write)
  │   ├── pwm.rs          (prescaler + channel register writes)
  │   ├── adc.rs          (ADC command + read)
  │   ├── servo.rs        (angle ↔ pulse, TTL lease watchdog)
  │   ├── battery.rs      (ADC A4 → voltage)
  │   ├── motor.rs        (H-bridge dir GPIO + PWM duty; TTL lease watchdog)
  │   ├── gpio.rs         (named pin abstraction; D2/D3/MCURST/SpeakerEn…)
  │   └── ultrasonic.rs   (HC-SR04 TRIG/ECHO GPIO timing)
  ├── calibration.rs      (CalibrationStore: motor/servo/grayscale calibration)
  ├── routine/
  │   ├── mod.rs          (RoutineEngine, RoutineState, RoutineStats)
  │   └── explore.rs      (explore_task: ultrasonic + normalised grayscale sensor-actuator loop)
  ├── reset.rs            (MCU reset via BCM5)
  └── testing.rs          (shared test mocks: MockI2c, MockGpio, MockAlsaControl — #[cfg(test)] only)
```

### Dependency Rules

1. `ipc/handler.rs` depends on `hat/` modules — it is the only bridge.
2. `hat/` sub-modules depend only on `hat/i2c.rs` for bus access.
3. `config.rs` has no internal dependencies.
4. `reset.rs` uses `rppal::gpio` directly (BCM5 is not on HAT I2C).
5. No module depends on `main.rs` — all logic is in `lib.rs`.

## Data Flow

### Servo Command

```
HatClient (Python)
  → JSON: {"id":"1","method":"set_servo_angle","params":{"channel":0,"angle_deg":90.0,"ttl_ms":500}}
  → Unix socket write + \n
  → ipc/mod.rs (read line from stream)
  → ipc/schema.rs (deserialize Request)
  → ipc/handler.rs (match method → call hat::servo::set_angle)
  → hat/servo.rs (convert angle → pulse_us, register TTL lease)
  → hat/pwm.rs (calculate prescaler, write channel)
  → hat/i2c.rs (rppal I2C write to 0x14)
  → Response: {"id":"1","ok":true,"result":{"channel":0,"angle_deg":90.0,"pulse_us":1500}}
  → Unix socket write + \n
  → HatClient reads, returns to Python
```

### Battery Read

```
HatClient (Python)
  → {"id":"2","method":"get_battery_voltage","params":{}}
  → ipc → handler → hat::battery::read_voltage()
  → hat::adc::read_channel(A4)
  → hat::i2c::write + read (0x14)
  → raw_adc: (raw / 4095) × 3.3 × 3.0 = voltage_v
  → {"id":"2","ok":true,"result":{"voltage_v":7.42}}
```

### Servo TTL Watchdog

```
set_servo_angle(ch=0, ttl_ms=500)
  → hat/servo.rs registers lease: (channel=0, expires=now+500ms)
  → watchdog task (polls every watchdog_poll_ms):
      - checks all active leases
      - if lease.expires < now:
          → hat/pwm.rs write pulse_us=0 (idle channel)
          → remove lease
          → log warning: "servo lease expired, channel 0 idled"
```

## IPC Protocol

See `nomothetic/docs/hat_ipc_schema.md` for the full specification.

**Transport**: Unix domain socket, `SOCK_STREAM`
**Framing**: NDJSON (one JSON object per line, `\n` terminated)
**Max message**: 4096 bytes
**Encoding**: UTF-8

### Methods Summary

| Method | Params | Result |
|--------|--------|--------|
| `health` | — | `schema_version`, `status`, `version`, `hat_address`, `i2c_bus`, `uptime_s` |
| `get_battery_voltage` | — | `voltage_v`, `raw_adc` |
| `set_servo_pulse_us` | `channel`, `pulse_us`, `ttl_ms` | `channel`, `pulse_us` |
| `set_servo_angle` | `channel`, `angle_deg`, `ttl_ms` | `channel`, `angle_deg`, `pulse_us` |
| `get_servo_status` | — | `active_leases: [{channel, ttl_remaining_ms, conn_id}]` |
| `reset_mcu` | — | `reset_ms` |
| `get_mcu_status` | — | `resets_since_start`, `last_reset_s_ago` |
| `read_adc` | `channel` (0–7) | `channel`, `raw_value` |
| `set_motor_speed` | `channel` (0–3), `speed_pct`, `ttl_ms` | `channel`, `speed_pct` |
| `stop_all_motors` | — | `stopped` (count) |
| `get_motor_status` | — | `active_leases: [{channel, ttl_remaining_ms, conn_id}]` |
| `drive` | `speed_pct`, `ttl_ms` | `speed_pct`, `motors` |
| `steer` | `angle_deg`, `ttl_ms` | `servo`, `channel`, `angle_deg`, `pulse_us` |
| `pan_camera` | `angle_deg`, `ttl_ms` | `servo`, `channel`, `angle_deg`, `pulse_us` |
| `tilt_camera` | `angle_deg`, `ttl_ms` | `servo`, `channel`, `angle_deg`, `pulse_us` |
| `read_grayscale` | — | `channels: [u8; 3]`, `values: [u16; 3]` |
| `read_ultrasonic` | — | `distance_cm` |
| `enable_speaker` | — | `enabled: true`, `pin_bcm` |
| `disable_speaker` | — | `enabled: false`, `pin_bcm` |
| `set_volume` | `volume_pct` (0–100) | `volume_pct` |
| `get_volume` | — | `volume_pct` |
| `set_mic_gain` | `gain_pct` (0–100) | `gain_pct` |
| `get_mic_gain` | — | `gain_pct` |
| `get_calibration` | — | `motors: [...]`, `servos: {...}`, `grayscale: [...]` |
| `set_motor_calibration` | `channel`, `speed_scale?`, `deadband_pct?`, `reversed?` | `channel`, `speed_scale`, `deadband_pct`, `reversed` |
| `set_servo_calibration` | `servo`, `trim_us` | `servo`, `trim_us` |
| `calibrate_grayscale` | `channel` (0–2), `surface` | `channel`, `adc_channel`, `surface`, `raw_value`, `stored` |
| `read_grayscale_normalized` | — | `channels: [u8; 3]`, `normalized: [f64; 3]` |
| `save_calibration` | — | `saved`, `path` |
| `reset_calibration` | — | `reset` |
| `start_routine` | `name`, `speed_pct?`, `obstacle_threshold_cm?`, `cliff_threshold_normalized?`, `max_duration_s?` | `name`, `started_at_uptime_s` |
| `stop_routine` | — | `name`, `ran_for_s`, `obstacles_avoided`, `cliffs_avoided`, `stop_reason` |
| `get_routine_status` | — | `running`, `name?`, `elapsed_s?`, `obstacles_avoided?`, `cliffs_avoided?` |
| `read_gpio` | `pin` | `pin`, `high` |
| `write_gpio` | `pin`, `high` | `pin`, `high` |

## Wi-Fi Soft AP

When the device is not connected to a known Wi-Fi network, `scripts/ap-mode.sh`
activates a WPA2 hotspot (`nomon-<last4-of-MAC>`) allowing the nomotactic app
to reach nomothetic at `http://192.168.4.1:8080` (plain HTTP, interface-bound
to the AP gateway). The passphrase is read from `/var/lib/nomon/ap_passphrase`
(a long random secret nomothetic generates, distinct from the 8-digit pairing
code; `ap-mode.sh` falls back to `pairing_secret` only for older nomothetic). See `systemd/nomon-softap.service`
and `systemd/nomon-softap-watchdog.timer` for the service units. Refer to
nomopractic ADR-005 (and nomothetic ADR-016 amendment) for the full design rationale.

### End-to-End Provisioning Sequence

1. **Device boots off-network** → `nomon-softap.service` calls `ap-mode.sh up`
   → hotspot `nomon-<last4>` broadcasts at `192.168.4.1`
2. **User connects** phone or laptop to `nomon-<last4>` using the Soft AP
   passphrase shown at `/run/nomothetic/ap-passphrase` as the Wi-Fi password
3. **User opens nomotactic app** → reaches nomothetic at `http://192.168.4.1:8080`
4. **Pairing** → user submits pairing secret to `POST /api/device/auth/pair`
   → nomothetic issues a device JWT
5. **Wi-Fi provisioning** → app reveals `WifiProvisionForm`; user enters home
   SSID + WPA2 password → app calls `POST /api/device/network/configure`
6. **Background connection** → nomothetic launches `nmcli device wifi connect
   <ssid> password <pwd> ifname wlan0` as an `asyncio` background task; returns
   `{"status":"connecting"}` immediately. NM stores a persistent connection
   profile so the device reconnects automatically on future boots.
7. **AP teardown** → `nomon-softap-watchdog.service` polls `nmcli general
   connectivity` every 30 s; when it reaches `full`, calls `ap-mode.sh down`
8. **Subsequent boots** → NM connects to home network immediately; AP only
   activates if home network is unreachable

### Error Codes

| Code | Meaning |
|------|---------|
| `UNKNOWN_METHOD` | Method name not recognized |
| `INVALID_PARAMS` | Missing or out-of-range parameters |
| `HARDWARE_ERROR` | I2C/SPI/GPIO bus failure |
| `NOT_READY` | Daemon still initializing |
| `SERVO_LEASE_EXPIRED` | TTL elapsed, servo was idled |
| `ALREADY_RUNNING` | A routine is already active; stop it before starting a new one |
| `INTERNAL_ERROR` | Unexpected daemon bug |

## Concurrency Model

- **Tokio async runtime** with multi-threaded scheduler.
- One spawned task per client connection (read lines → dispatch → respond).
- HAT I2C bus access serialized via `tokio::sync::Mutex` (one bus transaction at
  a time — required by I2C protocol).
- Servo TTL watchdog runs as a background `tokio::spawn` task, polling on
  `watchdog_poll_ms` interval.
- Client disconnect triggers cleanup: release servo leases for that client.

## Configuration

Priority order (highest wins):

1. **CLI arguments** (`--config <path>`)
2. **Environment variables** (`NOMON_HAT_*`)
3. **Config file** (TOML)
4. **Compiled defaults**

See `config.toml` for all options.

## Security

- Socket created with mode `0660`, group `nomon`.
- Daemon runs as root (required for I2C/GPIO), socket restricted to `nomon` group.
- No network listeners — Unix socket only (kernel-enforced access control).
- Wi-Fi Soft AP: WPA2-PSK with a ~116-bit passphrase from
  `/var/lib/nomon/ap_passphrase` (generated by nomothetic on first run, never
  hardcoded, independent of the short pairing code). See ADR-005.
- Servo TTL lease prevents stall on client crash.
- Input validation on all IPC parameters (channel range, pulse range, etc.).

## Deployment

- **Binary**: Cross-compiled for `aarch64-unknown-linux-gnu`, deployed to
  `/usr/local/bin/nomopractic`.
- **Config**: `/etc/nomopractic/config.toml`.
- **Service**: `systemd/nomopractic.service` → `systemctl enable nomopractic`.
- **Socket dir**: `/run/nomopractic/` created by `ExecStartPre` in systemd unit.
- **Updates**: Binary download + SHA-256 verify + atomic swap + `systemctl restart`.
