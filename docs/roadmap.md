# nomopractic — Development Roadmap

## Overview

nomopractic is the Rust HAT hardware daemon for the nomon fleet. Development is
organized into phases, each building on the previous. This aligns with Phase 5
milestones in the nomothetic roadmap.

---

## Phase 1 — Foundation & IPC Scaffold

**Goal**: Minimal daemon that listens on a Unix socket, parses NDJSON, and
responds to `health` requests. No hardware access yet.

### 1.1 — Project Bootstrap
- [x] Cargo project initialized with dependencies
- [x] Source module tree scaffolded
- [x] systemd unit file
- [x] Example config (TOML)
- [x] Copilot instructions
- [x] Architecture & roadmap docs

### 1.2 — Configuration
- [x] `config.rs`: Load TOML config file with serde
- [x] Environment variable overrides (`NOMON_HAT_*`)
- [x] CLI argument parsing (clap): `--config <path>`
- [x] Validation: reject invalid log_level, zero TTL/watchdog
- [x] Unit tests: 9 tests (defaults, file load, env overrides, validation errors)

### 1.3 — Tracing & Logging
- [x] `tracing-subscriber` init with `env-filter`
- [x] Log level from config / `RUST_LOG` env var
- [x] Structured fields in log output

### 1.4 — IPC Listener
- [x] `ipc/mod.rs`: Tokio `UnixListener` on configured socket path
- [x] Socket permissions (mode `0660`)
- [x] Per-client task spawning with graceful shutdown (ctrl-c)
- [x] Read NDJSON lines (max 4096 bytes), parse `Request`
- [x] Write `Response` + newline back to client
- [x] Client disconnect cleanup (log)
- [x] Integration tests: 5 tests (health, unknown method, malformed JSON,
      multiple requests, concurrent clients)

### 1.5 — Health Method
- [x] `ipc/handler.rs`: Route `health` method with uptime tracking
- [x] Response: `schema_version`, `status`, `version`, `uptime_s`, `hat_address`, `i2c_bus`
- [x] Error response for unknown methods (`UNKNOWN_METHOD`)
- [x] Error response for malformed JSON (`INVALID_PARAMS`)
- [x] Unit tests: 5 tests (health, unknown method, malformed, missing field, default params)

### Phase 1 Exit Criteria
- `cargo test` — all tests pass (no hardware required)
- `cargo clippy -- -D warnings` — zero warnings
- `cargo fmt --check` — clean
- Daemon starts, accepts socket connections, responds to `health`
- Can be verified with `socat`:
  ```
  echo '{"id":"1","method":"health","params":{}}' | \
    socat - UNIX-CONNECT:/run/nomopractic/nomopractic.sock
  ```

---

## Phase 2 — I2C & Battery Voltage (P0)

**Goal**: Read battery voltage from the Robot HAT V4 via I2C. First real
hardware interaction.

### 2.1 — I2C Helpers
- [x] `hat/i2c.rs`: Open I2C bus via rppal
- [x] `read_register(addr, reg, buf)` helper
- [x] `write_register(addr, reg, data)` helper
- [x] Shared `Hat` struct holding `rppal::i2c::I2c` behind `tokio::sync::Mutex`
- [x] Unit tests with mock I2C (trait-based abstraction)

### 2.2 — ADC Read
- [x] `hat/adc.rs`: Write command byte, read 2-byte result
- [x] Channel validation (A0–A7)
- [x] Error handling for I2C failures

### 2.3 — Battery Voltage
- [x] `hat/battery.rs`: Read ADC channel A4
- [x] Scaling: `voltage_v = (raw / 4095) × 3.3 × 3.0` (12-bit ADC, 3.3 V ref, 3:1 voltage divider)
- [x] `get_battery_voltage` IPC method wired up in handler
- [x] Unit test: mock ADC → verify voltage calculation

### Phase 2 Exit Criteria
- [x] Battery voltage readable via IPC
- [x] I2C errors returned as `HARDWARE_ERROR` to client
- [x] All tests pass without hardware (mocked I2C)

---

## Phase 3 — PWM & Servo Control (P0)

**Goal**: Drive servos on PWM channels 0–11. Includes TTL lease safety watchdog.

### 3.1 — PWM Control
- [x] `hat/pwm.rs`: Prescaler calculation from `CLOCK_HZ` / `SERVO_FREQ`
- [x] PWM initialization: set prescaler + auto-reload registers
- [x] Channel pulse write: `REG_CHN + channel * 4`
- [x] Frequency validation (50 Hz default for servos)

### 3.2 — Servo Abstraction
- [x] `hat/servo.rs`: Angle → pulse_us conversion
  - `pulse_us = 500 + (angle / 180) × 2000`
- [x] `set_servo_pulse_us` IPC method
- [x] `set_servo_angle` IPC method
- [x] Channel validation (0–11), pulse range validation (500–2500 µs)
- [x] Angle range validation (0–180°)

### 3.3 — TTL Lease Watchdog
- [x] Per-channel lease tracking: `(channel, expires_at)`
- [x] Background watchdog task (polls every `watchdog_poll_ms`)
- [x] Expired lease → idle channel (pulse_us = 0)
- [x] Client disconnect → release all leases for that client
- [x] Warning log on lease expiry
- [x] Unit tests for watchdog timing

### Phase 3 Exit Criteria
- [x] Servo commands work via IPC
- [x] TTL watchdog idles channels on expired leases
- [x] Client disconnect cleans up leases
- [x] All tests pass without hardware

---

## Phase 4 — GPIO & MCU Reset (P1)

**Goal**: Named GPIO pin abstraction and MCU reset capability.

### 4.1 — GPIO Pins
- [x] `hat/gpio.rs`: Named pin enum (D4, D5, MCURST, SW, LED)
- [x] BCM pin mapping
- [x] Direction configuration (input/output)
- [x] Read/write operations

### 4.2 — MCU Reset
- [x] `reset.rs`: Assert BCM5 low for ≥ 10 ms, release high
- [x] `reset_mcu` IPC method
- [x] Response includes `reset_ms` duration
- [x] Safety: debounce / rate-limit reset requests

### Phase 4 Exit Criteria
- [x] GPIO readable/writable via IPC
- [x] MCU reset works via IPC
- [x] All tests pass without hardware

---

## Phase 5 — Hardening & Deployment

**Goal**: Production-ready daemon with CI, cross-compilation, and deploy tooling.

### 5.1 — CI Pipeline
- [x] GitHub Actions workflow: test, clippy, fmt, cross-compile (`.github/workflows/ci.yml`)
- [x] Cross-compile for `aarch64-unknown-linux-gnu` (via `cross`)
- [x] Binary artifact uploaded to GitHub Releases (on `v*` tags via `softprops/action-gh-release`)

### 5.2 — Deploy Script
- [x] `scripts/deploy.sh`: Download binary, verify SHA-256, atomic swap, restart service
- [x] Version pinning in script arguments (`./deploy.sh <version> [<pi-host>]`)
- [x] Rollback support (keep previous binary as `nomopractic.bak`)

### 5.3 — Integration Testing on Pi
- [x] Integration testing complete (v0.1.0 results below)
- [x] End-to-end test: start daemon → connect via socket → verify HAT responses
- [x] Battery voltage sanity check (voltage in expected range)
- [x] Servo sweep test (0° → 180° → 0°)
- [x] MCU reset test
- [x] Wi-Fi Soft AP pairing complete (Phase 15; BLE pairing removed — see ADR-005)

**Results (v0.1.0, 2026-03-10, Pi Zero 2W / PicarX):**

| Test | Result | Notes |
|------|--------|-------|
| T1 Health | PASS | `status:ok`, `schema_version:1.0.0`, `hat_address:0x14` |
| T2 Battery voltage | PASS | `raw:3329`, `voltage_v:8.06 V` (2S LiPo, healthy) |
| T3 Servo sweep | PASS | P0: 0°→180°→90°, pulses 500/2500/1500 µs, physical movement confirmed |
| T4 MCU reset | PASS | `reset_ms:10` |
| T5 Post-reset health | PASS | Daemon survived reset, `uptime_s:934` |

**Bugs found and fixed during testing:**
- ADC command byte was `0x10 + channel`; correct formula is `0x10 \| (7 - channel)` (robot-hat register map)
- Battery scaling was `raw × 3`; correct formula is `(raw / 4095) × 3.3 × 3.0` (12-bit ADC, 3.3 V ref, 3:1 divider)
- Single-shot `socat` kills servo immediately via TTL-on-disconnect; use a persistent connection for servo testing

### 5.4 — Raw ADC IPC Method
- [x] `read_adc` IPC method: expose raw ADC reads for all channels A0–A7
- [x] Channel validation (0–7), `INVALID_PARAMS` on out-of-range
- [x] `HARDWARE_ERROR` propagated on I2C failure
- [x] Response: `{ channel, raw_value }`
- [x] Unit tests: valid channel, invalid channel, raw value passthrough

### 5.5 — Code Consolidation
- [x] Deduplicate `MAX_CHANNEL = 11`: export as `pub const` from `pwm.rs`, import in `servo.rs`

### 5.6 — Daemon State Methods
- [x] `get_servo_status` IPC method: active lease list with `channel`, `ttl_remaining_ms`, `conn_id`
- [x] `get_mcu_status` IPC method: `resets_since_start` counter, `last_reset_s_ago` (null if never reset)
- [x] `reset_count` and `last_reset_at` unified under a single `Mutex<McuState>` in `Handler`
- [x] Unit tests for both methods

### 5.7 — nomothetic Integration
- [x] Implement `nomothetic.hat.HatClient` in Python repo
- [x] Add HAT REST endpoints to `nomothetic.api` (GET /api/hat/battery, POST /api/hat/servo, POST /api/hat/reset)
- [x] End-to-end: REST → HatClient → Unix socket → nomopractic → I2C → HAT
- [x] Mock-socket tests in nomothetic (20 tests in `tests/test_hat.py`)

### Phase 5 Exit Criteria
- CI green on every push
- Binary deployable to Pi via script
- nomothetic REST endpoints work end-to-end with nomopractic

---

## Phase 6 — DC Motor Control (P1)

**Goal**: Drive PicarX DC wheels (and up to 4 motors generically) via the
Robot HAT V4 TC1508S H-bridge driver. Includes TTL lease watchdog (same
safety model as servos) and config-driven wiring.

**Pre-requisite bug fix (6.0)**: The existing `set_channel_pulse_us` register
formula `REG_CHN + channel * 4` is only correct for channel 0; channels 1–11
compute the wrong register address, and channels 12–13 collide with timer
config registers. The correct formula (from the SunFounder reference
implementation) is `REG_CHN + channel`. Discovered during Phase 6 analysis;
fixed as the first step of this phase.

### 6.0 — PWM Register Formula Fix (prerequisite)
- [x] `hat/pwm.rs`: Fix `set_channel_pulse_us` register: `REG_CHN + channel`
      (was `REG_CHN + channel * 4`)
- [x] Fix `init_pwm` to initialize timers 0–2 (channels 0–11, stride-1 per
      timer group) instead of only timers 0 and 4
- [x] Add `init_motor_pwm(hat, freq_hz)` — initializes timer 3 (channels 12–15)
- [x] Add `set_motor_channel_duty_pct(hat, channel, pct)` — percentage-based
      duty write for motor channels 12–15 (bypasses servo pulse-width path)
- [x] Add `MOTOR_FREQ`, `MOTOR_MIN_CHANNEL`, `MOTOR_MAX_CHANNEL` constants
- [x] Update all affected unit tests

### 6.1 — Motor Config
- [x] `config.rs`: `MotorConfig { pwm_channel, dir_pin_bcm, reversed }` struct
- [x] `motors: Vec<MotorConfig>` array (up to 4 entries) in `Config`
- [x] `motor_default_ttl_ms: u64` field in `Config`
- [x] Default: PicarX 2-motor wiring
  - Motor 0: `pwm_channel=12`, `dir_pin_bcm=24` (D5)
  - Motor 1: `pwm_channel=13`, `dir_pin_bcm=23` (D4)
- [x] Validation: `pwm_channel` ∈ 12–15, max 4 motors, `motor_default_ttl_ms > 0`
- [x] Update `apply_env_overrides` and config tests

### 6.2 — GPIO BCM Helper
- [x] `hat/gpio.rs`: `write_gpio_bcm(gpio, bcm, high)` — drives an arbitrary
      BCM output pin (used by motor driver for config-specified direction pins)

### 6.3 — Motor Driver (`hat/motor.rs`)
- [x] New module `hat/motor.rs`
- [x] `set_motor_speed(hat, gpio, pwm_channel, dir_pin_bcm, reversed, speed_pct)`:
  - `speed_pct` clamped to −100.0–+100.0 (negative = reverse)
  - Direction computed as `forward = (speed_pct >= 0) XOR reversed`
  - Writes direction GPIO before PWM duty (avoid momentary wrong-direction torque)
- [x] `idle_motor(hat, pwm_channel)` — zero duty without touching direction pin
- [x] `MotorError { Hat(HatError), Gpio(GpioError) }` error type
- [x] Unit tests: forward, backward, stop, reversed flag, speed clamping,
      invalid channel rejection

### 6.4 — Motor IPC Methods
- [x] `set_motor_speed`: `{ channel, speed_pct, ttl_ms? }` — IPC channel 0–3
      maps to `config.motors[channel]`
- [x] `stop_all_motors`: `{}` — idle all configured motors, clear motor leases
- [x] `get_motor_status`: returns active motor leases
- [x] Wired up in `ipc/handler.rs` with `motor_lease_manager: Arc<LeaseManager>`
- [x] Motor channels idled on client disconnect (same pattern as servos)
- [x] `motor_error_code()` helper for IPC error classification

### 6.5 — Motor Watchdog
- [x] `servo.rs`: add `revoke_channel(channel)` to `LeaseManager`
- [x] `ipc/mod.rs`: poll motor leases in `watchdog_task`; call `idle_motor` on expiry

### 6.6 — Startup Init
- [x] `main.rs`: call `pwm::init_motor_pwm` when `config.motors` is non-empty

### Phase 6 Exit Criteria
- [x] `set_motor_speed` drives wheels via IPC with signed-percentage control
- [x] TTL watchdog stops motors on lease expiry
- [x] Client disconnect stops all held motor channels
- [x] All tests pass without hardware
- [x] `config.toml` documents motor wiring

---

## Phase 7 — Vehicle Convenience Methods (P1)

**Goal**: High-level IPC methods that operate on named peripherals (steering
servo, camera servos, all motors together, grayscale sensors) using a single,
coordinated IPC call. Channel-to-peripheral mappings are defined in
`config.toml` so the daemon is the single source of truth.

### 7.1 — Named Peripheral Config
- [x] `config.rs`: `ServoChannels { camera_pan, camera_tilt, steering }` struct
  - Each field is `Option<u8>`, allowing individual servos to be disabled
  - Defaults: `camera_pan=Some(0)`, `camera_tilt=Some(1)`, `steering=Some(2)`
  - Validation: channel must be in 0–11 range when `Some`
- [x] `config.rs`: `SensorChannels { grayscale: [u8; 3] }` struct
  - Default: `[0, 1, 2]` (A0 = left, A1 = centre, A2 = right for PicarX)
  - Validation: each channel must be in 0–7 range
- [x] Both structs added to `Config`; `config.toml` updated with
      `[servos]` and `[sensors]` sections

### 7.2 — Drive IPC Method
- [x] `drive { speed_pct, ttl_ms? }`: set all configured motors simultaneously
  - Atomic — no inter-motor gap or round trips
  - Returns `{ speed_pct, motors: N }`

### 7.3 — Named Servo IPC Methods
- [x] `steer { angle_deg, ttl_ms? }`: set steering servo (`config.servos.steering`)
- [x] `pan_camera { angle_deg, ttl_ms? }`: set camera pan (`config.servos.camera_pan`)
- [x] `tilt_camera { angle_deg, ttl_ms? }`: set camera tilt (`config.servos.camera_tilt`)
- [x] All three return `{ servo, channel, angle_deg, pulse_us }`
- [x] `INVALID_PARAMS` returned if the named servo is disabled (`None`)

### 7.4 — Grayscale Sensor IPC Method
- [x] `read_grayscale {}`: read all three grayscale ADC channels in one call
  - Channel indices taken from `config.sensors.grayscale`
  - Returns `{ channels: [u8; 3], values: [u16; 3] }`

### 7.5 — Tests
- [x] config.rs: 6 new unit tests for ServoChannels/SensorChannels validation
- [x] handler.rs: 10 new unit tests for all 5 new IPC methods

### Phase 7 Exit Criteria
- [x] All 5 new IPC methods work via socket
- [x] Named peripheral channels configurable in `config.toml`
- [x] All tests pass without hardware (138 total)
- [x] `cargo clippy -- -D warnings` clean
- [x] `cargo fmt --check` clean

---

## Priority Legend

| Label | Meaning |
|-------|---------|
| **P0** | Must-have for initial deployment |
| **P1** | Important but not blocking deployment |
| **P2** | Future enhancement |

## Current Status

| Phase | Name | Status | Tests |
|-------|------|--------|-------|
| 1 | Foundation & IPC Scaffold | ✅ Complete | 19 |
| 2 | I2C & Battery Voltage | ✅ Complete | 31 |
| 3 | PWM & Servo Control | ✅ Complete | 62 |
| 4 | GPIO & MCU Reset | ✅ Complete | 82 |
| 5 | Hardening & Deployment | ✅ Complete | 89 |
| 6 | DC Motor Control | ✅ Complete | 112 |
| 7 | Vehicle Convenience Methods | ✅ Complete | 138 |
| 8 | Peripheral Expansion | ✅ Complete | 149 |
| 9 | Audio Levels Control | ✅ Complete | 168 |
| 10 | Calibration & Configuration | ✅ Complete | 206 |
| 11 | Routine Engine | ✅ Complete | 222 |
| 13 | BLE GATT Server | ⊘ Superseded by Phase 15 | 278 |
| 13.1 | BLE Simplification | ⊘ Superseded by Phase 15 | — |
| 14 | Service Env-File & Deploy Hardening | ✅ Complete | — |
| 15 | BLE → Wi-Fi Soft AP Migration | ✅ Complete | — |
| 16 | Odometry, IMU & UWB Raw Inputs; `cmd_vel` | 🔜 Planned — autonomon ADR-008 | — |

**Test total (current): 239 passing** (201 lib + 38 integration; BLE tests from Phase 13 removed in Phase 15)

---

Developer pairing notes:
- `docs/pairing.md` — **DELETED in Phase 15** (BLE developer guide, superseded by Soft AP).
  The shared pairing secret at `/var/lib/nomon/pairing_secret` is now dual-purpose: it is
  displayed at nomothetic startup (HTTP pairing) and also used as the WPA2 password for the
  `nomon-<last4-of-MAC>` Soft AP hotspot. See `docs/adr/005-wifi-soft-ap.md`.

## Phase 8 — Peripheral Expansion (P1)

**Goal**: Add support for the Robot HAT V4 ultrasonic distance sensor and
speaker amplifier enable pin. Expands the GPIO pin table and adds three new
IPC methods backed by config-driven pin assignments.

### 8.1 — GPIO Pin Expansion
- [x] `hat/gpio.rs`: Added `D2` (BCM 27), `D3` (BCM 22), `SpeakerEn` (BCM 20)
  to `GpioPin` enum
- [x] `bcm()`, `name()`, `from_name()` match arms updated for new variants
- [x] `is_output()` updated: `D3` (ECHO, input) excluded alongside `Sw`

### 8.2 — Ultrasonic Sensor (`hat/ultrasonic.rs`)
- [x] New module `hat/ultrasonic.rs`
- [x] `read_distance_cm(gpio, trig_bcm, echo_bcm, timeout_ms) -> Result<f64, UltrasonicError>`:
  - Quiesce TRIG (low, sleep 1 ms) → pulse TRIG high for 10 µs → wait ECHO
    high → time ECHO pulse → compute `elapsed_s × 34330 / 2`
  - Valid range: 2–400 cm (HC-SR04 spec); out-of-range returns `NoEcho`
- [x] `UltrasonicError { Gpio(GpioError), Timeout(u64), NoEcho }`
- [x] 3 unit tests

### 8.3 — UltrasonicConfig
- [x] `config.rs`: `UltrasonicConfig { trig_pin_bcm: u8, echo_pin_bcm: u8, timeout_ms: u64 }`
  - Defaults: TRIG = BCM 27 (D2), ECHO = BCM 22 (D3), timeout = 20 ms
- [x] `speaker_en_pin_bcm: u8` field added to `Config` (default: 20 = BCM 20)
- [x] `config.toml` updated with `[ultrasonic]` section

### 8.4 — IPC Methods
- [x] `read_ultrasonic {}` → `{ distance_cm: f64 }`
  - Calls `ultrasonic::read_distance_cm` with config-specified pins/timeout
  - `HARDWARE_ERROR` on GPIO failure; `TIMEOUT` on measurement timeout; `NO_ECHO` when object is out of sensor range (2–400 cm)
- [x] `enable_speaker {}` → `{ enabled: true, pin_bcm: 20 }`
  - Writes BCM 20 (`SpeakerEn`) high via GPIO bus
- [x] `disable_speaker {}` → `{ enabled: false, pin_bcm: 20 }`
  - Writes BCM 20 low

### 8.5 — Tests
- [x] `ipc/handler.rs`: 3 unit tests — `enable_speaker_returns_enabled_true`,
      `disable_speaker_returns_enabled_false`, `enable_then_disable_speaker_toggles_pin`

### Phase 8 Exit Criteria
- [x] All 3 new IPC methods dispatched correctly
- [x] GPIO pin table complete for PicarX sensors & speaker
- [x] All 149 tests pass without hardware
- [x] `cargo clippy -- -D warnings` clean
- [x] `cargo fmt --check` clean

---

### v0.3.0 Release Prep

**Goal**: Align config strategy with `nomothetic`, remove legacy files, and
confirm checks pass before tagging.

- [x] `config.toml.example` renamed to `config.toml` — defaults committed to
      repo; no copy step required at install
- [x] `docs/releases/` removed (GitHub auto-generates release notes from tags)
- [x] Version bumped to `0.3.0` in `Cargo.toml`
- [x] `cargo clippy -- -D warnings` clean
- [x] `cargo fmt --check` clean
- [x] All tests pass

---

## Completed

### Phase 9 — Audio Levels Control (P1)

**Goal**: Expose software control for both output volume (HifiBerry DAC) and
input gain (USB microphone PCM2902) via new IPC methods, allowing the nomothetic
API to manage audio input and output levels without restarting the daemon.

**Output Volume (HifiBerry DAC — ALSA card 1):**
- [x] `hat/audio.rs`: `AlsaControl` trait + `AmixerControl` implementation using `std::process::Command` to invoke `amixer`
- [x] `set_volume { volume_pct: u8 }` IPC method (0–100)
- [x] `get_volume {}` IPC method returning `{ volume_pct: u8 }`
- [x] Config: `[audio]` section in `config.toml` with `output_card_index`, `output_control`, `default_volume_pct`

**Input Gain (USB Microphone PCM2902 — ALSA card 2):**
- [x] `set_mic_gain { gain_pct: u8 }` IPC method (0–100)
- [x] `get_mic_gain {}` IPC method returning `{ gain_pct: u8 }`
- [x] Config: `input_card_index`, `input_control`, `default_mic_gain_pct` in `[audio]` section

**Testing & Integration:**
- [x] Unit tests for all four IPC methods (MockAlsaControl — no real hardware)
- [x] `cargo test` — 142 lib + 26 integration tests passing
- [x] `cargo clippy -- -D warnings` clean
- [x] Phase 9 exit: both output volume and input gain settable via IPC without daemon restart

---

### Phase 10 — Calibration & Configuration (P1)

**Goal**: Allow all motors, servos, and sensors to be adjusted and calibrated at
runtime via IPC, and persisted to a dedicated `calibration.toml` file. Ensures
the robot behaves correctly before autonomous routines are engaged.

**Architecture**: A `CalibrationStore` holds live-mutable calibration values
separate from the static `Config`. The store is loaded from `calibration.toml`
at startup (falling back to defaults if absent) and written back on
`save_calibration`. Changes take effect immediately; no daemon restart required.

#### 10.0 — CalibrationStore (`src/calibration.rs`)
- [x] `MotorCalibration { speed_scale: f64, deadband_pct: f64, reversed: bool }` per motor channel
  - `speed_scale`: multiplier on `speed_pct` before PWM write (range 0.5–2.0, default 1.0)
  - `deadband_pct`: minimum duty % below which motor does not spin (range 0.0–20.0, default 0.0)
  - `reversed`: runtime-adjustable direction flip (independent of `MotorConfig.reversed`)
- [x] `GrayscaleCalibration { white_raw: u16, black_raw: u16 }` per sensor position
  - 3-element fixed array aligned to `config.sensors.grayscale` positions [left=0, center=1, right=2]
  - Defaults: `white_raw = 100`, `black_raw = 3000`; validated `white_raw < black_raw`
- [x] `ServoCalibration { trim_us: i16 }` per logical servo name
  - `trim_us`: added to computed `pulse_us` before 500–2500 clamping (range −500–+500, default 0)
- [x] `CalibrationStore { motors: Vec<MotorCalibration>, grayscale: [GrayscaleCalibration; 3], servos: HashMap<String, ServoCalibration> }`
- [x] Held in `Handler` behind `Arc<tokio::sync::Mutex<CalibrationStore>>`
- [x] `CalibrationStore::load_or_default(path)`: loads from TOML file; file absence is not an error
- [x] Validation: `speed_scale` ∈ 0.5–2.0; `deadband_pct` ∈ 0.0–20.0; `|trim_us|` ≤ 500; `white_raw < black_raw`
- [x] `config.rs`: `calibration_path: PathBuf` (default `"/etc/nomopractic/calibration.toml"`; env var `NOMON_HAT_CALIBRATION_PATH`)
- [x] `config.toml` updated with `calibration_path` entry

#### 10.1 — Apply Calibration to Hardware Paths
- [x] `ipc/handler.rs`: apply `MotorCalibration` in `set_motor_speed` and `drive` dispatch:
  - `effective_speed_pct = clamp(speed_pct × speed_scale, −100.0, 100.0)`
  - If `|effective_speed_pct| < deadband_pct`, set to 0 (motor stays stopped)
  - Apply `calibration.reversed XOR config.reversed` for final direction
- [x] `ipc/handler.rs`: apply `ServoCalibration.trim_us` in `steer`, `pan_camera`, `tilt_camera`:
  - `effective_pulse_us = clamp(computed_pulse_us + trim_us, 500, 2500)`
- [x] Calibration `Mutex` guard acquired, value copied, guard dropped before any hardware `.await` (no deadlocks)

#### 10.2 — Normalised Grayscale
- [x] `read_grayscale_normalized {}` IPC method:
  - Reads raw ADC values (reuses `read_grayscale` hardware path via `config.sensors.grayscale`)
  - Per-channel: `normalized = clamp((raw − white_raw) / (black_raw − white_raw), 0.0, 1.0)`
  - Returns `{ channels: [u8; 3], normalized: [f64; 3] }` (0.0 = white/reflective, 1.0 = black/non-reflective)
  - `channels` mirrors `read_grayscale` — the ADC channel numbers from `config.sensors.grayscale`
- [x] Note for Phase 11: `RoutineConfig` will gain `cliff_threshold_normalized: f64` (default 0.7); explore routine uses normalised threshold when calibration is present

#### 10.3 — Calibration IPC Methods
- [x] `get_calibration {}` → full snapshot:
  - `motors: [{ channel, speed_scale, deadband_pct, reversed }, ...]` — indexed 0…N-1 matching `config.motors`
  - `servos: { "steering": { trim_us }, "camera_pan": { trim_us }, "camera_tilt": { trim_us } }`
  - `grayscale: [{ adc_channel, white_raw, black_raw }, ...]` — 3 elements; `adc_channel` taken from `config.sensors.grayscale[i]`
- [x] `set_motor_calibration { channel, speed_scale?, deadband_pct?, reversed? }` → `{ channel, speed_scale, deadband_pct, reversed }`
  - Partial updates: unspecified fields unchanged
  - `INVALID_PARAMS` if `channel` ≥ `config.motors.len()`
- [x] `set_servo_calibration { servo, trim_us }` → `{ servo, trim_us }`
  - `servo` must be `"steering"`, `"camera_pan"`, or `"camera_tilt"`; `INVALID_PARAMS` otherwise
  - Calibration stored regardless of whether that servo is currently enabled (`None`) in config
- [x] `calibrate_grayscale { channel, surface }` → `{ channel, adc_channel, surface, raw_value, stored: bool }`
  - `channel`: sensor position index (0 = left, 1 = center, 2 = right); **not** the ADC bus channel
  - Actual ADC read uses `config.sensors.grayscale[channel]` for the bus channel
  - `surface`: `"white"` or `"black"`; reads live ADC and stores as `white_raw` or `black_raw`
  - Returns `INVALID_PARAMS` if the resulting `white_raw ≥ black_raw` would violate the constraint
  - `stored` is `false` (and error is returned) when the constraint would be violated
- [x] `save_calibration {}` → `{ saved: true, path: "/etc/nomopractic/calibration.toml" }` — writes current store to `calibration_path`
- [x] `reset_calibration {}` → `{ reset: true }` — reverts in-memory store to defaults (file not overwritten until next `save_calibration`)
- [x] All 7 new methods added to `nomothetic/docs/hat_ipc_schema.md` (authoritative IPC contract)

#### 10.4 — Tests
- [x] `src/calibration.rs`: default values, `load_or_default` round-trip (write TOML → reload → compare),
  validation errors (`speed_scale` out of range, `white_raw ≥ black_raw`),
  partial motor update, reset to defaults (~8 tests)
- [x] `ipc/handler.rs`: `get_calibration` defaults; `set_motor_calibration` partial update (speed_scale only);
  `set_motor_calibration` invalid channel; `set_servo_calibration` valid; `set_servo_calibration` invalid name;
  `calibrate_grayscale` white capture; `calibrate_grayscale` black capture; `calibrate_grayscale` constraint violation;
  `save_calibration`; `reset_calibration`; `read_grayscale_normalized` with defaults;
  `read_grayscale_normalized` with custom calibration (~12 tests)

#### 10.5 — Documentation
- [x] `nomothetic/docs/hat_ipc_schema.md`: add full method specs for all 7 new IPC methods
  (`get_calibration`, `set_motor_calibration`, `set_servo_calibration`, `calibrate_grayscale`,
  `read_grayscale_normalized`, `save_calibration`, `reset_calibration`)
- [x] `nomopractic/docs/architecture.md` Methods Summary table: add Phase 9 audio level methods
  and all Phase 10 calibration and normalised grayscale methods
- [x] `nomothetic/docs/architecture.md` endpoints table: add Phase 9 audio level endpoints
  and all Phase 10 calibration + `GET /api/sensor/grayscale/normalized` endpoints

#### Phase 10 Exit Criteria
- [x] Motor calibration (speed_scale, deadband, direction) applied transparently to all motor commands
- [x] Servo trim applied transparently to all named servo commands
- [x] `read_grayscale_normalized` returns 0.0–1.0 values based on captured surface references
- [x] Calibration persisted to and reloaded from `calibration.toml` across daemon restarts
- [x] All tests pass without hardware
- [x] `cargo test` — 206 tests passing (171 lib + 35 integration)
- [x] `cargo clippy -- -D warnings` clean
- [x] `cargo fmt --check` clean

---

## Upcoming

### Phase 11 — Routine Engine (P1)

**Goal**: Run self-contained hardware-loop routines entirely within the daemon.
Routines survive IPC client disconnects — they are started, stopped, and queried
via three new IPC methods. The first routine is `explore`: drive forward, avoid
obstacles with the ultrasonic sensor, and avoid cliffs with the grayscale sensors.

**Architecture decision:** Routines live in nomopractic (Rust) rather than
nomothetic (Python) so the sensor-actuator loop runs with zero network
round-trips per iteration and continues operating even when the REST API client
is not connected. The normal TTL watchdog safety model is preserved: the routine
task continuously refreshes motor leases; if the task panics or is stopped, the
watchdog idles all motors within `ttl_ms` milliseconds.

#### 11.0 — RoutineConfig
- [x] `config.rs`: `RoutineConfig { explore_speed_pct: f64, obstacle_threshold_cm: f64, cliff_threshold_normalized: f64, loop_interval_ms: u64, avoidance_backup_ms: u64, avoidance_turn_angle_deg: f64 }`
  - Defaults: speed `30.0`, obstacle `25.0 cm`, cliff_normalized `0.7` (0.0 = white/reflective, 1.0 = black/non-reflective), loop `100 ms`, backup `500 ms`, turn `60°`
- [x] `[routine]` TOML section added to `config.toml`
- [x] Validation: speed in 1.0–100.0, `obstacle_threshold_cm > 0`, `cliff_threshold_normalized` ∈ 0.0–1.0, `loop_interval_ms ≥ 50`

#### 11.1 — Routine Module (`routine/`)
- [x] `src/routine/mod.rs`: `RoutineEngine`, `RoutineState` enum (`Idle | Running | Stopping`), `RoutineStats { obstacles_avoided, cliffs_avoided }`
- [x] `RoutineEngine` holds `Arc<Hat>`, `Arc<HatGpio>`, `Arc<Config>`, `Arc<tokio::sync::Mutex<CalibrationStore>>`, `Arc<LeaseManager>` (motor leases)
  - `CalibrationStore` ref needed so `explore_task` can apply normalised cliff detection using live calibration
- [x] Stop signal: `Arc<std::sync::atomic::AtomicBool>` (no new dependencies)
- [x] `ROUTINE_CONN_ID: u64 = 0` constant — pseudo-connection ID for routine-owned motor leases
- [x] `start(name, params) -> Result<(), RoutineError>`: spawns `tokio::spawn` task; returns `ALREADY_RUNNING` if occupied
- [x] `stop() -> Option<RoutineStats>`: sets stop flag, awaits `JoinHandle` (with 2 s timeout), stops all motors
- [x] `status() -> RoutineStatusSnapshot`: `{ running, name, elapsed_s, stats }`

#### 11.2 — Explore Routine (`routine/explore.rs`)
- [x] `explore_task(hat, gpio, motor_lease_manager, config, params, stats, stop_flag)` async fn
- [x] Loop at `loop_interval_ms` (default 100 ms):
  1. Check stop flag and `max_duration_s` — exit if either triggered
  2. Read ultrasonic distance (`read_distance_cm`)
  3. Read normalised grayscale: compute `(raw − white_raw) / (black_raw − white_raw)` per channel using live `CalibrationStore` values (0.0 = white/reflective, 1.0 = black/non-reflective)
  4. **Cliff detected** (`any normalized_value ≥ cliff_threshold_normalized`): stop motors → reverse `avoidance_backup_ms` → steer away from the most-dark channel → resume straight; increment `cliffs_avoided`
  5. **Obstacle detected** (`distance ≤ obstacle_threshold_cm` or ultrasonic timeout): stop motors → reverse `avoidance_backup_ms` → steer `avoidance_turn_angle_deg` right → resume straight; increment `obstacles_avoided`
  6. **Clear**: `drive(speed_pct, ttl_ms=2000)` + `steer(90°, ttl_ms=2000)`
- [x] On task exit (any reason): call `stop_all_motors` + clear motor leases
- [x] Ultrasonic read errors treated as obstacle (fail-safe)

#### 11.3 — IPC Methods
- [x] `start_routine { name, speed_pct?, obstacle_threshold_cm?, cliff_threshold_normalized?, max_duration_s? }` → `{ name, started_at_uptime_s }`
  - `name` must be a known routine name (`"explore"`); `INVALID_PARAMS` otherwise
  - `ALREADY_RUNNING` error code returned if a routine is active
  - Per-call params override config defaults (not persisted)
- [x] `stop_routine {}` → `{ name, ran_for_s, obstacles_avoided, cliffs_avoided, stop_reason: "commanded" | "timeout" | "error" }`
  - `INVALID_PARAMS` if no routine is running
- [x] `get_routine_status {}` → `{ running: bool, name: string | null, elapsed_s: integer | null, obstacles_avoided: integer | null, cliffs_avoided: integer | null }`
- [x] All three wired up in `ipc/handler.rs`; `RoutineEngine` held in `Handler` behind `Arc<tokio::sync::Mutex<RoutineEngine>>`
- [x] New error code `ALREADY_RUNNING` added to IPC schema error code table

#### 11.4 — Safety
- [x] `max_duration_s` param (default: 300 s) auto-stops the routine after the time limit; `stop_reason: "timeout"`
- [x] Task panic — if `JoinHandle` returns `Err`, `stop()` logs `error!` and still stops all motors; `stop_reason: "error"`
- [x] Mutex guard over `RoutineEngine` is dropped before every `await` in the handler (no deadlocks)
- [x] Routine cannot starve the IPC handler — it runs on a separate Tokio task

#### 11.5 — Tests
- [x] `routine/mod.rs`: unit tests — start (idle→running), double-start rejected, stop (running→idle), status (idle), status (running)
- [x] `routine/explore.rs`: unit tests with mocked sensor reads — obstacle→reverse→turn sequence, cliff→reverse sequence, clear→drive-straight, max_duration exit
- [x] `ipc/handler.rs`: unit tests — `start_routine` success, `start_routine` unknown name, `start_routine` ALREADY_RUNNING, `stop_routine` success, `stop_routine` not-running, `get_routine_status` idle, `get_routine_status` running

#### Phase 11 Exit Criteria
- [x] `start_routine { "name": "explore" }` navigates autonomously until stopped
- [x] `stop_routine` arrests all motors and returns telemetry stats
- [x] Routine continues through IPC client disconnect; motors stop on explicit `stop_routine` or timeout
- [x] All tests pass without hardware
- [x] `cargo test` — 222 tests passing (184 unit + 38 integration)
- [x] `cargo clippy -- -D warnings` clean
- [x] `cargo fmt --check` clean

---

### Phase 13 — BLE GATT Server ⊘ Superseded by Phase 15

**Goal**: Expose a BLE GATT server from the nomopractic daemon so mobile
clients can discover, pair with, and send basic commands to the robot without
WiFi connectivity. All BLE commands bridge to the existing IPC handler —
no hardware logic is duplicated.

**Architecture decisions:**
- ADR-001: BLE GATT server in nomopractic
- ADR-002: Binary protocol for BLE GATT
- ADR-003: BLE security model

**Cross-repo dependencies:**
- nomothetic Phase 18: shared pairing secret, BlueZ prerequisites
- nomotactic Phase 2: BLE client implementation

**Hardware:** BCM43436s (Pi Zero 2W) — BLE 4.2, shared antenna with WiFi.
Practical indoor range: 10–30 m. BLE MTU: 20–244 bytes (negotiated).

#### 13.1 — BLE Infrastructure
- [x] Add `bluer` crate (BlueZ D-Bus bindings) behind `ble` Cargo feature flag
- [x] Add crypto crates: `jsonwebtoken`, `hkdf`, `sha2`, `aes`, `ccm`
      behind same `ble` feature flag
- [x] `config.rs`: `BleConfig` struct:
  - `enabled: bool` (default `false`)
  - `device_name: String` (default `"nomon"`)
  - `pairing_secret_path: PathBuf` (default `/var/lib/nomon/pairing_secret`)
  - `jwt_secret_env: String` (default `NOMON_JWT_SECRET`)
- [x] `[ble]` section in `config.toml`
- [x] Validation: `device_name` length ≤ 29 bytes (BLE advertising limit)

#### 13.2 — Binary Protocol Codec (`ble/protocol.rs`)
- [x] Frame types: `BleRequest`, `BleResponse` with `opcode`, `seq_nr`,
      `length`, `payload` fields
- [x] Opcode enum: `Heartbeat(0x01)`, `GetBattery(0x02)`,
      `SetMotorSpeed(0x03)`, `StopAllMotors(0x04)`, `SetServoAngle(0x05)`,
      `Drive(0x06)`, `Steer(0x07)`, `ReadUltrasonic(0x08)`,
      `ReadGrayscale(0x09)`, `GetHealth(0x0A)`
- [x] Response opcodes: request opcode | 0x80; `Error = 0xFF`
- [x] `encode_response(resp) -> Vec<u8>` and `decode_request(bytes) -> Result<BleRequest>`
- [x] Fixed-point helpers: `speed_x100`, `angle_x10`, `voltage_mv`, `distance_x10`
- [x] Little-endian throughout
- [x] Unit tests: round-trip encode/decode for every opcode, boundary values,
      truncated input, invalid opcode

#### 13.3 — BLE GATT Service Definitions (`ble/services.rs`)
- [x] Register GATT application via `bluer` with 4 services:

**nomon Pairing Service** (`e3a10001-7b2a-4b9c-8f5a-2b7d6e4f1a3c`)

| Characteristic | UUID | Properties | Description |
|----------------|------|------------|-------------|
| Pairing Secret | `e3a11001-…` | Write | Client writes pairing secret (UTF-8) |
| Auth Token | `e3a11002-…` | Read, Notify | Server sends `salt (16B) \|\| JWT` after pairing |
| Session State | `e3a11003-…` | Read, Notify | `0x00`=unpaired, `0x01`=paired, `0x02`=error |

**nomon Command Service** (`e3a10002-7b2a-4b9c-8f5a-2b7d6e4f1a3c`)

| Characteristic | UUID | Properties | Description |
|----------------|------|------------|-------------|
| Command Write | `e3a12001-…` | Write | Client writes binary command frame |
| Command Response | `e3a12002-…` | Notify | Server sends binary response frame |

**nomon WiFi Provisioning** (`e3a10003-7b2a-4b9c-8f5a-2b7d6e4f1a3c`)

| Characteristic | UUID | Properties | Description |
|----------------|------|------------|-------------|
| WiFi Command | `e3a13001-…` | Write | `0x01`=scan, `0x02 \|\| ssid_len \|\| ssid \|\| pwd_len \|\| pwd`=connect, `0x03`=status |
| WiFi Result | `e3a13002-…` | Read, Notify | Result type prefix + result payload |

**nomon Status Service** (`e3a10004-7b2a-4b9c-8f5a-2b7d6e4f1a3c`)

| Characteristic | UUID | Properties | Description |
|----------------|------|------------|-------------|
| Device State | `e3a14001-…` | Read, Notify | `0x00`=idle, `0x01`=driving, `0x02`=routine |
| Battery Level | `e3a14004-…` | Read, Notify | `voltage_mv: u16 LE` (periodic notify) |

- [x] BLE advertising with device name and Pairing Service UUID
- [x] MTU negotiation (request 247 for 244-byte ATT payload)

#### 13.4 — BLE Pairing & Session Auth (`ble/session.rs`)
- [x] `BleSession` struct: `session_key: [u8; 16]`, `client_counter: u16`,
      `server_counter: u16`, `paired: bool`, `jwt: String`
- [x] `handle_pairing_write(secret_bytes)`: read pairing secret from
      `config.ble.pairing_secret_path`, constant-time compare via
      `hmac::digest::subtle::ConstantTimeEq`, single-use consumption
- [x] On success: generate 16-byte random salt, derive `session_key` via
      HKDF-SHA256 (`ikm=secret, salt=random_salt, info="nomon-ble-session"`,
      `len=16`)
- [x] Issue JWT (HS256) with claims `{sub:"device-owner@local",
      iss:"nomon-device", exp:<now+24h>, iat:<now>}`
- [x] Notify Auth Token: `salt(16) || jwt_bytes`
- [x] Notify Session State: `0x01` (paired)
- [x] `encrypt_response(plain, session) -> Vec<u8>`: AES-128-CCM encrypt,
      increment `server_counter`
- [x] `decrypt_command(cipher, session) -> Result<Vec<u8>>`: AES-128-CCM
      decrypt, verify `client_counter > last_seen`, reject replay
- [x] Session teardown on BLE disconnect
- [x] Unit tests: pairing success/failure, key derivation determinism,
      encrypt/decrypt round-trip, replay rejection, counter wrap

#### 13.5 — BLE Command Bridge (`ble/bridge.rs`)
- [x] `handle_ble_command(frame, handler, session) -> BleResponse`:
  - If not paired and opcode ≠ health/status: return `NotAuthenticated`
  - Decrypt frame payload using `session.decrypt_command()`
  - Decode binary request via `protocol::decode_request()`
  - Map BLE opcode → IPC method name + params JSON
  - Call `handler.dispatch(method, params)` (reuse existing handler)
  - Map IPC result JSON → binary response via `protocol::encode_response()`
  - Encrypt response via `session.encrypt_response()`
- [x] TTL lease: BLE commands use a dedicated `BLE_CONN_ID: u64 = 1`
      (distinct from routine's `ROUTINE_CONN_ID = 0`)
- [x] Unit tests: dispatch for each opcode, auth rejection, error mapping

#### 13.6 — WiFi Provisioning (`ble/wifi.rs`)
- [x] `scan_wifi() -> Vec<WifiNetwork>`: shell out to
      `nmcli -t -f SSID,SIGNAL,SECURITY dev wifi list` (same `Command` pattern
      as `amixer` in `hat/audio.rs`)
- [x] `connect_wifi(ssid, password) -> Result<(), WifiError>`: shell out to
      `nmcli dev wifi connect <ssid> password <password>`
- [x] `wifi_status() -> WifiState`: shell out to `nmcli -t -f STATE,DEVICE,CONNECTION general`
- [x] `WifiState` enum: `Disconnected`, `Connecting`, `Connected { ssid, rssi_dbm }`
- [x] `WifiError`: `ScanFailed`, `ConnectionFailed(String)`, `CommandFailed(String)`
- [x] Notify WiFi Status characteristic on state changes
- [x] Unit tests with mocked `Command` output (same pattern as `MockAlsaControl`)

#### 13.7 — Connection Monitoring
- [x] `ble/mod.rs`: detect BLE client disconnect via `bluer` events
- [x] On disconnect: clear BLE session, idle all `BLE_CONN_ID` motor/servo
      leases (same pattern as IPC client disconnect)
- [x] Integrate with existing TTL watchdog — BLE motor commands carry `ttl_ms`,
      watchdog idles on expiry
- [x] Status Service: update Device State characteristic on state transitions
- [x] Battery Level characteristic: periodic notify every 30 s (configurable)

#### 13.8 — Startup & Lifecycle
- [x] `main.rs`: if `config.ble.enabled`, spawn BLE GATT server task
- [x] Graceful shutdown: deregister GATT application, stop advertising
- [x] Log: `info!(device_name, "BLE GATT server started")`
- [x] Log: `warn!` if BlueZ D-Bus connection fails (BLE disabled gracefully)

#### Phase 13 Exit Criteria
- [x] BLE GATT server advertises and accepts connections
- [x] BLE pairing with secret verification + session key derivation works
- [x] Motor, servo, sensor commands work over BLE with binary protocol
- [x] WiFi credentials exchangeable over BLE; Pi connects to WiFi
- [x] BLE disconnect idles all motors/servos with BLE leases
- [x] AES-128-CCM encryption on all post-pairing command frames
- [x] All tests pass without BlueZ (`ble` feature flag disabled in CI)
- [x] `cargo test` — all existing + new tests pass
- [x] `cargo clippy -- -D warnings` clean
- [x] `cargo fmt --check` clean

---

### Phase 13.1 — BLE Simplification: Native OS Pairing + JSON Relay ⊘ Superseded by Phase 15

**Goal**: Replace the custom BLE binary protocol, AES-128-CCM encryption, and
pairing ceremony with OS-level Bluetooth passkey pairing and plain NDJSON relay.
Reduces ~2,900 lines of BLE code to ~600 lines while making every existing and
future IPC method automatically available over BLE.

**Architecture decisions:**
- ADR-004: BLE Simplification — Native OS Pairing + JSON Relay
- Supersedes: ADR-002 (Binary Protocol), ADR-003 (BLE Security Model)
- Preserves: ADR-001 (BLE GATT Server in nomopractic)

**Cross-repo dependencies:**
- nomothetic Phase 18.1: update IPC schema docs, pairing secret format change
  (numeric passkey instead of arbitrary string)
- nomotactic Phase 2.1: simplified BLE client

**Scope exclusion:** `reset_pairing` (physical button to clear bonds and
regenerate passkey) is a future enhancement, not part of this phase.

#### 13.1.0 — Delete Superseded Modules
- [x] Delete `src/ble/protocol.rs` — binary codec (885 lines, 32 tests)
- [x] Delete `src/ble/session.rs` — AES-128-CCM + HKDF + JWT issuance (467 lines, 10 tests)
- [x] Update `src/ble/mod.rs` — remove `pub mod protocol;`, `pub mod session;`
- [x] Verify: `cargo test` passes (remaining modules may have compile errors;
      fix in subsequent steps)

#### 13.1.1 — Cargo Dependency Cleanup
- [x] `Cargo.toml`: move `jsonwebtoken` from optional (`ble` feature) to
      always-included dependency (needed by `authenticate` IPC method)
- [x] `Cargo.toml`: remove from optional deps: `hkdf`, `sha2`, `aes`, `ccm`,
      `subtle`, `rand`
- [x] `Cargo.toml`: simplify `ble` feature: `ble = ["dep:bluer", "dep:tokio-stream"]`
- [x] Verify: `cargo build` and `cargo build --features ble` both succeed

#### 13.1.2 — WiFi Module Extraction
- [x] Create `src/wifi.rs` — move `WifiControl` trait, `NmcliWifi` struct,
      `WifiNetwork`/`WifiStatus` types, and `parse_scan_output`/`parse_status_output`
      from `src/ble/wifi.rs` (not behind `ble` feature flag)
- [x] Remove binary encoding functions from extracted code:
      `encode_scan_result`, `encode_wifi_status`, `decode_wifi_command`,
      `WifiCommand` enum (binary variant), `WifiResult` enum
- [x] Delete `src/ble/wifi.rs`
- [x] Update `src/lib.rs` — add `pub mod wifi;`
- [x] Move WiFi tests: keep nmcli parsing/mock tests, remove binary encoding tests
- [x] Verify: `cargo test` passes

#### 13.1.3 — WiFi IPC Methods
- [x] `ipc/handler.rs`: add `wifi_scan` method:
      - Params: `{}` (none)
      - Instantiate `NmcliWifi` (or use trait-injected `WifiControl`)
      - Call `wifi_control.scan()`
      - Result: `{ networks: [{ ssid, signal, security }, ...] }`
      - Error: `HARDWARE_ERROR` on scan failure
- [x] `ipc/handler.rs`: add `wifi_connect` method:
      - Params: `{ ssid: string, password: string }`
      - Validate: `ssid` and `password` must be non-empty strings
      - Call `wifi_control.connect(ssid, password)`
      - Result: `{ success: true }`
      - Error: `INVALID_PARAMS` on missing fields, `HARDWARE_ERROR` on failure
- [x] `ipc/handler.rs`: add `wifi_status` method:
      - Params: `{}` (none)
      - Call `wifi_control.status()`
      - Result: `{ state: "disconnected" | "connected", ssid: string | null, signal: integer | null }`
      - Error: `HARDWARE_ERROR` on query failure
- [x] `Handler` struct: add `wifi_control: Arc<dyn WifiControl>` field
      (default: `Arc::new(NmcliWifi)`, test: mock)
- [x] Unit tests: 6 tests (each method: success + error case)

#### 13.1.4 — Authenticate IPC Method
- [x] `ipc/handler.rs`: add `authenticate` method (transport-agnostic —
      available on both BLE and Unix socket; explicit team decision):
      - Params: `{}` (none)
      - Read JWT secret from `NOMON_JWT_SECRET` env var; if missing, return
        `{ code: "INTERNAL_ERROR", message: "JWT secret not configured" }`
      - Issue JWT (HS256): claims `{ sub: "device-owner@local", iss: "nomon-device",
        exp: now + 86400, iat: now }`
      - Result: `{ token: "eyJ...", expires_at: "<RFC3339 timestamp>" }`
- [x] Define `BLE_CONN_ID: u64 = 1` in `ipc/handler.rs` (not behind feature flag;
      replaces definition in `ble/bridge.rs`)
- [x] Unit tests: 3 tests (success via BLE conn_id, success via Unix socket,
      missing JWT secret)

#### 13.1.5 — Simplified GATT Services (`ble/services.rs`)
- [x] Rewrite `services.rs` — single GATT service with 2 characteristics:
  - **nomon Service** (`e3a10001-7b2a-4b9c-8f5a-2b7d6e4f1a3c`)
  - **Command Write** (`e3a12001-...`): Write property. Client writes NDJSON
    request chunks. Server accumulates in a per-connection buffer until `\n`,
    then dispatches via `Handler::dispatch()`.
  - **Response Notify** (`e3a12002-...`): Notify property. Server sends NDJSON
    response chunks. If response exceeds (MTU − 3) bytes, split into chunks
    and send as sequential notifications.
- [x] Remove all references to pairing, WiFi, and status services
- [x] Remove all characteristic UUIDs except `COMMAND_WRITE_CHAR` and
      `COMMAND_RESPONSE_CHAR`; keep `PAIRING_SERVICE_UUID` as the single
      service UUID (renamed conceptually to "nomon Service")
- [x] `GattHandles` struct simplified: one write receiver + one notify control
- [x] `build_gatt_application()` returns simplified application
- [x] `run_characteristic_io()` rewritten: read NDJSON from write channel,
      dispatch via handler, send response via notify control; handle chunking

#### 13.1.6 — Simplified Bridge (`ble/bridge.rs`)
- [x] Rewrite `bridge.rs` — JSON passthrough instead of binary translation:
  - Receive raw bytes from Command Write characteristic
  - Accumulate in buffer until `\n` (newline-terminated NDJSON)
  - Forward complete JSON line to `Handler::dispatch(json_line, BLE_CONN_ID)`
  - Receive JSON response string from handler
  - Chunk response at (MTU − 3) boundary, send via Response Notify
- [x] Keep `BLE_CONN_ID` re-exported from `ipc/handler.rs`
- [x] Remove all binary codec imports and response mapping logic
- [x] Unit tests: 4 tests (single-chunk request, multi-chunk request,
      single-chunk response, multi-chunk response)

#### 13.1.7 — BlueZ Passkey Agent (`ble/mod.rs`)
- [x] Rewrite `start_ble_server()`:
  - Read 6-digit numeric passkey from `config.ble.pairing_secret_path`
  - Parse as `u32` (validate range 0–999999)
  - Register BlueZ passkey agent via `bluer::agent::Agent`:
    - `request_passkey` callback returns the stored passkey
    - Agent capability: `KeyboardDisplay` (supports Passkey Entry)
  - Build simplified GATT application (1 service, 2 characteristics)
  - Start LE advertising with device name and service UUID
  - Spawn characteristic I/O handler task (simplified bridge)
  - On shutdown: clear session, release BLE leases, stop advertising
- [x] Remove `SessionState` usage and session imports
- [x] Simplify `BleError` — remove session-related variants
- [x] Connection monitoring: detect BLE client disconnect via `bluer` events;
      idle all `BLE_CONN_ID` leases on disconnect (existing pattern preserved)

#### 13.1.8 — Config Simplification
- [x] `config.rs`: `BleConfig` struct reduced to:
      - `enabled: bool` (default `false`)
      - `device_name: String` (default `"nomon"`)
      - `pairing_secret_path: PathBuf` (default `/var/lib/nomon/pairing_secret`)
      - Remove `jwt_secret_env` field (JWT secret now read directly from
        `NOMON_JWT_SECRET` env var in the `authenticate` handler)
- [x] `config.toml`: update `[ble]` section to match (remove `jwt_secret_env`)
- [x] Config validation: `device_name` ≤ 29 bytes (unchanged)
- [x] Unit tests: update config validation tests

#### 13.1.9 — Test Cleanup & New Tests
- [x] Remove binary protocol tests (~32 tests in `protocol.rs` — file deleted)
- [x] Remove crypto/session tests (~10 tests in `session.rs` — file deleted)
- [x] Remove binary WiFi encoding tests (~10 of ~23 tests in old `wifi.rs`)
- [x] Keep WiFi nmcli parsing/mock tests (moved to `wifi.rs` at crate root)
- [x] Add JSON relay integration tests (5 tests):
      - Valid NDJSON request → correct NDJSON response
      - Malformed JSON → error response
      - Multi-chunk request assembly
      - Multi-chunk response splitting
      - `authenticate` method from both BLE and Unix socket conn_ids
- [x] Add WiFi IPC method tests (6 tests, step 13.1.3)
- [x] Add `authenticate` tests (3 tests, step 13.1.4)

#### 13.1.10 — Documentation
- [x] Update `docs/architecture.md`:
      - BLE section: replace binary protocol description with NDJSON relay
      - Methods Summary: add `wifi_scan`, `wifi_connect`, `wifi_status`, `authenticate`
      - Module Dependency Graph: update `ble/` tree (remove `protocol.rs`, `session.rs`)
      - Security section: replace AES-128-CCM description with OS-level passkey pairing
- [x] Update `docs/adr/002-ble-binary-protocol.md`: add "Superseded by ADR-004" status
- [x] Update `docs/adr/003-ble-security-model.md`: add "Superseded by ADR-004" status
- [x] Verify: `docs/roadmap.md` Phase 13.1 entry is complete and consistent

#### Phase 13.1 Exit Criteria
- [x] BLE GATT server advertises single service with 2 characteristics
- [x] OS-level passkey pairing works (BlueZ agent returns numeric passkey)
- [x] NDJSON commands dispatched identically to Unix socket IPC
- [x] `authenticate` method returns valid JWT over BLE
- [x] `wifi_scan`, `wifi_connect`, `wifi_status` work from both BLE and Unix socket
- [x] NDJSON chunking handles responses exceeding BLE MTU
- [x] BLE disconnect idles all `BLE_CONN_ID` motor/servo leases
- [x] No crypto crates in `ble` feature (`aes`, `ccm`, `hkdf`, `sha2`, `subtle`, `rand` removed)
- [x] All tests pass without BlueZ (`ble` feature flag disabled in CI)
- [x] Net test change: ~56 tests removed, ~19 tests added
- [x] `cargo test` — all tests pass
- [x] `cargo clippy -- -D warnings` clean
- [x] `cargo fmt --check` clean

---

## Phase 14 — Service Env-File & Deploy Hardening ✅

**Goal:** Fix the hardcoded `User=root` / `Group=nomon` in the service file by
converting it to an `envsubst` template, and extend `scripts/deploy.sh` to
write a filtered runtime env file and install the expanded service file on the Pi.

**Dependency:** None. No Rust code changes. No IPC changes.
**Cross-repo:** Paired with nomothetic Phase 19 (same pattern, independent fix).

---

### 14.1 — Service File Template

**File:** `nomopractic/systemd/nomopractic.service`

Add `EnvironmentFile=` and replace hardcoded `User=` / `Group=` with template
vars. The `${NOMON_SERVICE_USER}` / `${NOMON_SERVICE_GROUP}` placeholders are
**not** systemd env-var syntax — they are `envsubst` placeholders expanded by
`scripts/deploy.sh` at install time, before systemd reads the file.

Replace the current `[Service]` section with:

```ini
[Service]
Type=simple
EnvironmentFile=-/etc/nomopractic/nomopractic.env
User=${NOMON_SERVICE_USER}
Group=${NOMON_SERVICE_GROUP}
ExecStartPre=/bin/mkdir -p /run/nomopractic
ExecStart=/usr/local/bin/nomopractic --config /etc/nomopractic/config.toml
Restart=on-failure
RestartSec=2s
Environment=RUST_LOG=info
```

**Verify:** `grep -E '^User=|^Group=' nomopractic/systemd/nomopractic.service`
returns `User=${NOMON_SERVICE_USER}` and `Group=${NOMON_SERVICE_GROUP}`.

---

### 14.2 — Document Service Variables in `.env.example`

**File:** `nomopractic/.env.example`

Add after the existing `# Deployment` block (after `NOMON_GITHUB_REPO` comment):

```ini
# =============================================================================
# Runtime service identity (systemd)
# =============================================================================
# User and group the nomopractic daemon runs as.
# Substituted into the systemd unit by scripts/deploy.sh at install time via envsubst.
# Default: User=root (required for rppal I2C/GPIO), Group=nomon.
# NOMON_SERVICE_USER=root
# NOMON_SERVICE_GROUP=nomon
```

**Verify:** `grep NOMON_SERVICE nomopractic/.env.example` shows two commented lines.

---

### 14.3 — Deploy Script Changes

**File:** `nomopractic/scripts/deploy.sh`

Four additions applied in order. No existing logic is removed.

#### 14.3-A — Add `SCRIPT_DIR` / `REPO_DIR`

Immediately after `set -euo pipefail`, before the `# ── Constants` block:

```bash
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(dirname "${SCRIPT_DIR}")"
```

#### 14.3-B — Add `.env` loading block

After the `cleanup()`/`trap cleanup EXIT` lines, before `# ── Argument parsing`:

```bash
# ── Load .env ─────────────────────────────────────────────────────────────────
ENV_FILE="${REPO_DIR}/.env"
if [[ -f "${ENV_FILE}" ]]; then
    while IFS= read -r line || [[ -n "${line}" ]]; do
        line="${line#"${line%%[![:space:]]*}"}"
        [[ "${line}" =~ ^# || -z "${line}" ]] && continue
        key="${line%%=*}"
        val="${line#*=}"
        val="${val%%#*}"
        val="${val#"${val%%[![:space:]]*}"}"
        val="${val%"${val##*[![:space:]]}"}"
        val="${val#\"}" ; val="${val%\"}"
        val="${val#\'}" ; val="${val%\'}"
        case "${key}" in
            NOMON_PI_HOST|NOMON_SSH_KEY|NOMON_GITHUB_REPO|NOMON_SERVICE_USER|NOMON_SERVICE_GROUP) \
                export "${key}=${val}" ;;
        esac
    done < "${ENV_FILE}"
fi
```

#### 14.3-C — Add `copy_nomopractic_env()` and service file upload

After the `_rsh`/`_rscp`/`ON_REMOTE` block, before `echo "==> Installing binary..."`:

```bash
# ── Env file & service file ────────────────────────────────────────────────────
# Vars excluded from the Pi's system env file — deploy secrets, not runtime config.
_DEPLOY_EXCLUDE='^\s*(NOMON_PI_HOST|NOMON_SSH_KEY|NOMON_GITHUB_REPO)\s*='

copy_nomopractic_env() {
    if [[ ! -f "${ENV_FILE}" ]]; then
        echo "==> Warning: .env not found; skipping /etc/nomopractic/nomopractic.env." >&2
        return
    fi
    if [[ "${ON_REMOTE}" == true ]]; then
        echo "==> Writing /etc/nomopractic/nomopractic.env on remote host..."
        grep -vE "${_DEPLOY_EXCLUDE}" "${ENV_FILE}" | \
            ssh "${SSH_OPTS[@]}" "${PI_HOST}" \
                'sudo mkdir -p /etc/nomopractic && sudo tee /etc/nomopractic/nomopractic.env >/dev/null'
    else
        echo "==> Writing /etc/nomopractic/nomopractic.env locally..."
        sudo mkdir -p /etc/nomopractic
        grep -vE "${_DEPLOY_EXCLUDE}" "${ENV_FILE}" | \
            sudo tee /etc/nomopractic/nomopractic.env >/dev/null
    fi
}

copy_nomopractic_env

# Upload service file template so the Pi can run envsubst on it.
_SERVICE_FILE="${REPO_DIR}/systemd/nomopractic.service"
REMOTE_SERVICE_TMP=""
if [[ "${ON_REMOTE}" == true && -f "${_SERVICE_FILE}" ]]; then
    REMOTE_SERVICE_TMP="/tmp/nomopractic.service.$$"
    _rscp "${_SERVICE_FILE}" "${PI_HOST}:${REMOTE_SERVICE_TMP}"
fi
```

#### 14.3-D — Remote heredoc: add service file install

In the `_rsh bash <<REMOTE … REMOTE` block, make two changes:

**1.** At the top of the heredoc, alongside the existing `INSTALL_PATH=`,
`SERVICE=`, `REMOTE_TMP=`, `REMOTE_CONFIG_TMP=` variable injections, add:

```bash
REMOTE_SERVICE_TMP="${REMOTE_SERVICE_TMP}"
_DEF_SVC_USER="${NOMON_SERVICE_USER:-root}"
_DEF_SVC_GROUP="${NOMON_SERVICE_GROUP:-nomon}"
```

**2.** Before `echo "==> Restarting \${SERVICE}.service..."` inside the heredoc, add:

```bash
# Install service file with envsubst expansion of User= and Group=.
if [[ -n "\${REMOTE_SERVICE_TMP}" ]]; then
    echo "==> Installing nomopractic.service..."
    NOMON_SERVICE_USER="\${_DEF_SVC_USER}"
    NOMON_SERVICE_GROUP="\${_DEF_SVC_GROUP}"
    if [[ -f /etc/nomopractic/nomopractic.env ]]; then
        set -o allexport
        source /etc/nomopractic/nomopractic.env
        set +o allexport
    fi
    NOMON_SERVICE_USER="\${NOMON_SERVICE_USER:-root}"
    NOMON_SERVICE_GROUP="\${NOMON_SERVICE_GROUP:-nomon}"
    envsubst '$NOMON_SERVICE_USER $NOMON_SERVICE_GROUP' \
        < "\${REMOTE_SERVICE_TMP}" \
        | sudo tee /etc/systemd/system/nomopractic.service >/dev/null
    rm -f "\${REMOTE_SERVICE_TMP}"
    echo "==> Service file installed."
fi
```

#### 14.3-E — Local branch: add service file install

In the `else` branch (no `PI_HOST`), before
`echo "==> Restarting ${SERVICE}.service..."`, add:

```bash
# Install service file with envsubst expansion of User= and Group=.
if [[ -f "${_SERVICE_FILE}" ]]; then
    echo "==> Installing nomopractic.service..."
    if [[ -f /etc/nomopractic/nomopractic.env ]]; then
        set -o allexport
        # shellcheck source=/dev/null
        source /etc/nomopractic/nomopractic.env
        set +o allexport
    fi
    NOMON_SERVICE_USER="${NOMON_SERVICE_USER:-root}"
    NOMON_SERVICE_GROUP="${NOMON_SERVICE_GROUP:-nomon}"
    envsubst '$NOMON_SERVICE_USER $NOMON_SERVICE_GROUP' \
        < "${_SERVICE_FILE}" \
        | sudo tee /etc/systemd/system/nomopractic.service >/dev/null
fi
```

---

### Phase 14 Exit Criteria

- `nomopractic/systemd/nomopractic.service` contains `User=${NOMON_SERVICE_USER}`,
  `Group=${NOMON_SERVICE_GROUP}`, and `EnvironmentFile=-/etc/nomopractic/nomopractic.env`
- After `./scripts/deploy.sh --local`:
  - `cat /etc/nomopractic/nomopractic.env` contains no `NOMON_PI_HOST`,
    `NOMON_SSH_KEY`, or `NOMON_GITHUB_REPO` lines
  - `grep -E '^User=|^Group=' /etc/systemd/system/nomopractic.service` shows
    literal expanded values (e.g. `User=root`, `Group=nomon`), not template vars
  - `systemctl is-active nomopractic` → `active`
- `cargo clippy -- -D warnings` — no warnings (no Rust changes)
- `cargo fmt --check` — clean (no Rust changes)

---

## Phase 15 — BLE → Wi-Fi Soft AP Migration ✅

**Goal**: Remove all Bluetooth/BLE code from nomopractic and replace the BLE
pairing channel with a Wi-Fi Soft AP fallback. When the device cannot reach a
known Wi-Fi network, NetworkManager broadcasts a WPA2-protected hotspot
(`nomon-<last4-of-MAC>`) so users can pair via the existing HTTP flow in any
browser or the nomotactic app — no native modules, no OS bonding, no shared
antenna contention.

**ADR**: [ADR-005: Wi-Fi Soft AP as Proximity Pairing Channel](docs/adr/005-wifi-soft-ap.md)
**Supersedes**: Phase 13 (BLE GATT Server), Phase 13.1 (BLE Simplification)
**Cross-repo**: nomothetic Phase 20, nomotactic (no new phase — BLE removal only)

---

### 15.1 — Delete BLE Source Module

- [x] Delete `src/ble/mod.rs`, `src/ble/bridge.rs`, `src/ble/services.rs`
      (entire `src/ble/` directory)
- [x] Delete `src/wifi.rs`
- [x] `src/lib.rs`: remove `pub mod ble;` and `pub mod wifi;` declarations
- [x] `Cargo.toml`: remove `bluer`, `tokio-stream` optional dependencies and
      `[features] ble` section; remove `jsonwebtoken` and `chrono` crates
      (used exclusively by the deleted `authenticate` IPC method)
- Verify: `cargo build` succeeds with no `ble` or `wifi` module references

### 15.2 — Strip BLE from Config

- [x] `src/config.rs`: delete `BleConfig` struct, the `ble: BleConfig` field
      on `Config`, the `ble: BleConfig::default()` in `Config::default()`, all
      `NOMON_BLE_*` environment-variable override blocks, and the
      `ble.device_name` length validation in `Config::validate()`
- Verify: `cargo test` passes; `grep -r BleConfig src/` returns nothing

### 15.3 — Strip BLE from main.rs

- [x] `src/main.rs`: remove the entire `#[cfg(feature = "ble")]` block that
      spawns the BLE GATT server task
- Verify: `cargo build` produces a warning-free binary; diff shows only the
      `cfg(feature = "ble")` block removed

### 15.4 — Remove Dead IPC Methods

- [x] `src/ipc/handler.rs`: remove dispatch arms for `wifi_scan`,
      `wifi_connect`, `wifi_status`, and `authenticate`
- [x] `src/ipc/handler.rs`: delete `handle_wifi_scan`, `handle_wifi_connect`,
      `handle_wifi_status`, and `handle_authenticate` method bodies
- [x] `src/ipc/handler.rs`: remove `use crate::wifi::{NmcliWifi, WifiControl};`
      import
- [x] Delete any unit tests in `handler.rs` that cover these four methods
- Verify: `cargo test` still passes; `cargo clippy -- -D warnings` clean

### 15.5 — Delete BLE Documentation

- [x] Delete `docs/pairing.md` (BLE developer guide)
- [x] Delete `docs/adr/` — **no deletion**; ADR-001 through ADR-004 are updated
      with `Superseded by ADR-005` in their Status fields (already done)
- Verify: `ls docs/pairing.md` returns non-zero

### 15.6 — Wi-Fi Soft AP Script

- [x] Create `scripts/ap-mode.sh` — shell script managing NM hotspot lifecycle:
  - Subcommands: `up` (create + activate `nomon-ap` NM connection),
    `down` (deactivate + delete `nomon-ap`), `status` (print current state)
  - On `up`: reads `/var/lib/nomon/pairing_secret` for the WPA2 PSK;
    derives SSID suffix from `ip link show wlan0 | awk '/ether/{print $2}'`
    (last 4 hex chars of MAC, no colons, e.g. `3a2f`)
  - Sets Pi IP `192.168.4.1/24`, `ipv4.method shared` (NM provides DHCP +
    NAT for AP clients)
  - Idempotent: `up` is a no-op if `nomon-ap` is already active; `down` is
    a no-op if not present
- [x] Create `systemd/nomon-softap.service` — `Type=oneshot` unit that calls
      `scripts/ap-mode.sh up`; `RemainAfterExit=yes`; `After=NetworkManager.service`
- [x] Create `systemd/nomon-softap-watchdog.service` + `.timer` — polls
      `nmcli general connectivity` every 30 s; calls `ap-mode.sh up` when
      connectivity is `none` or `limited`, `ap-mode.sh down` when `full`
- [x] `scripts/deploy.sh`: install `nomon-softap.service`, `.timer`, and
      `nomon-softap-watchdog.service` to `/etc/systemd/system/` on deploy;
      enable and start timer
- Verify: `systemctl is-active nomon-softap-watchdog.timer` → `active`;
      on Pi with no known network, `nmcli con show nomon-ap` shows the hotspot

### 15.7 — Update Architecture Docs

- [x] `docs/architecture.md`: replace BLE section with Wi-Fi Soft AP section;
      update module dependency graph (remove `ble`, `wifi` module nodes);
      update Methods Summary table (remove `wifi_scan`, `wifi_connect`,
      `wifi_status`, `authenticate`)
- Verify: `grep -r "bluer\|BLE\|ble::" docs/` returns nothing substantive

#### Phase 15 Exit Criteria

- [x] `cargo build` — succeeds with zero `ble` or `wifi` module references
- [x] `cargo test` — all tests pass; no BLE-conditional tests remain
- [x] `cargo clippy -- -D warnings` — clean
- [x] `cargo fmt --check` — clean
- [x] `grep -r 'bluer\|BLE_CONN_ID\|BleConfig\|ble::' src/` — no output
- [x] `grep 'wifi_scan\|wifi_connect\|wifi_status\|authenticate' src/ipc/handler.rs` — no output
- [x] `scripts/ap-mode.sh up` + `scripts/ap-mode.sh down` succeed on Pi
- [x] When Pi has no known network: `nomon-<last4>` AP appears within 30 s,
      `http://192.168.4.1:8080/api/device/auth/status` is reachable from a
      connected client

### Phase 15.8 — Wi-Fi Credential Provisioning (cross-repo)

> **Note:** No nomopractic source changes. The `nomon-softap-watchdog.service`
> + `ap-mode.sh` already handle AP teardown once `nmcli general connectivity`
> reaches `full` — this is the mechanism that shuts down the AP after the user
> provisions home Wi-Fi via the nomothetic endpoint.

- [x] `nomon-softap-watchdog.service` handles AP teardown on full connectivity ✅
- [x] `docs/architecture.md`: updated to document end-to-end provisioning flow

**Cross-repo:** nomothetic Phase 20.4

---

## Phase 16 — Odometry, IMU & UWB Raw Inputs; `cmd_vel` (P0 for autonomy)

> Set by autonomon ADR-008 (2026-09-13). The brain needs a pose and a fast
> operator range before it can follow safely or retrace a route. These are
> pure **raw inputs** (ADR-004 boundary): nomopractic reads the hardware and
> exposes counts/rates; all fusion happens in autonomon.

### 16.1 — Wheel odometry
- [ ] `[odometry]` config: per-motor encoder (or hall) GPIO pins, ticks per
      revolution, wheel diameter, track width
- [ ] `hat/encoder.rs` — interrupt-driven tick counters behind a `GpioBus`
      trait (mock in tests)
- [ ] IPC `read_odometry` → `{ left_ticks, right_ticks, left_m, right_m,
      dt_ms, timestamp }`; counters are monotonic, reset on daemon start
- Verify: 1 m push test reports `left_m`/`right_m` within 2 %

### 16.2 — IMU
- [ ] `[imu]` config: I2C address, model (BNO055 / ICM-20948 / MPU-6050
      class), mounting orientation
- [ ] `hat/imu.rs` — driver behind the `I2cBus` trait; 20 Hz sampling task
- [ ] IPC `read_imu` → `{ gyro_radps: [x,y,z], accel_mps2: [x,y,z],
      heading_deg?: f64, timestamp }`
- Verify: stationary gyro bias < 0.01 rad/s after warm-up; 90° table turn
      integrates to 90 ± 3°

### 16.3 — UWB ranging
- [ ] `[uwb]` config: serial device, baud, module protocol (DW3000-class
      module reporting range / PDoA bearing)
- [ ] `hat/uwb.rs` — UART line parser; `read_uwb` → `{ range_cm,
      bearing_deg?: f64, quality, timestamp }`
- Verify: 1 m and 3 m tape-measure checks within 15 cm

### 16.4 — `cmd_vel` and chassis kinematics
- [ ] IPC `cmd_vel { v_mps, omega_radps, ttl_ms }` — converts body velocity
      to PicarX Ackermann steer angle + motor speed using the calibration
      store; same lease/watchdog semantics as `drive`/`steer`
- [ ] `[kinematics]` config: wheelbase, max steer angle, speed→duty map
- Verify: `cmd_vel {0.3, 0}` drives straight at ≈0.3 m/s over 2 m; a
      commanded arc closes within 10 % radius

### 16.5 — Reduce the Phase-11 routine engine to reflexes
- [ ] Remove the `explore` wandering behaviour from `src/routine/`; keep
      `stop_if_closer_than_cm` and `stop_on_cliff` as reflex guards that
      override any lease (the brain owns behaviour — autonomon ADR-004/008)
- [ ] `start_routine` accepts only the reflex set; nomothetic Phase 11 docs
      updated

#### Phase 16 Exit Criteria
- [ ] `cargo test` green with mocked encoder/IMU/UWB buses; `make check` clean
- [ ] `docs/hat_ipc_schema.md` (nomothetic) documents `read_odometry`,
      `read_imu`, `read_uwb`, `cmd_vel`
- [ ] Bench verification steps above recorded in `docs/hardware_reference.md`
