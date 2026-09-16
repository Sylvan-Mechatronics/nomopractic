# nomopractic

Low-latency HAT hardware daemon for the nomon fleet.

## What It Does

nomopractic is a Rust daemon that drives the SunFounder Robot HAT V4 on
Raspberry Pi Zero 2W microcontrollers. It exposes hardware controls over a Unix
domain socket using NDJSON framing, consumed by the Python
[nomothetic](https://github.com/Perceptua/nomothetic) package via its
`HatClient`.

**All hardware register logic lives here.** The Python side contains zero
hardware knowledge — it only sends/receives IPC messages.

## Capabilities

| Feature | Details |
|---------|---------|
| Battery voltage | ADC channel A4, scaled reading |
| Servo control | 12 PWM channels, angle or pulse-width, TTL safety lease |
| DC motor control | 4 channels, speed percentage, TTL safety lease |
| Named vehicle methods | `drive`, `steer`, `pan_camera`, `tilt_camera` |
| Grayscale sensors | 3 ADC channels, raw + normalized readings, calibration |
| Ultrasonic sensor | Distance measurement in cm |
| Audio | Speaker enable/disable, volume and mic gain via ALSA |
| Calibration | Per-motor, per-servo, and grayscale calibration; persist to TOML |
| Routines | Built-in obstacle-avoidance and line-following autonomous routines |
| MCU reset | Assert/release BCM5 GPIO |
| Named GPIO | D4, D5, MCURST, SW, LED — read/write via IPC |
| Wi-Fi Soft AP | WPA2 hotspot (`nomon-<last4-of-MAC>`) for proximity pairing (ADR-005) |
| IPC | Unix socket + NDJSON at `/run/nomopractic/nomopractic.sock` |
| Config | TOML file + `NOMON_HAT_*` env var overrides |

## Quick Start

```bash
# Build
cargo build

# Run tests (no hardware required)
cargo test

# Lint
cargo clippy -- -D warnings

# Cross-compile for Pi
cross build --target aarch64-unknown-linux-gnu --release
```

## Deployment

```bash
# On the Pi:
sudo cp target/aarch64-unknown-linux-gnu/release/nomopractic /usr/local/bin/
sudo mkdir -p /etc/nomopractic
sudo cp config.toml /etc/nomopractic/config.toml
sudo cp systemd/nomopractic.service /etc/systemd/system/
sudo systemctl enable --now nomopractic
```

Verify:

```bash
echo '{"id":"1","method":"health","params":{}}' | \
  socat - UNIX-CONNECT:/run/nomopractic/nomopractic.sock
```

## Project Structure

```
src/
├── main.rs          Binary entry point (CLI, config, runtime)
├── lib.rs           Library root
├── config.rs        TOML + env configuration
├── ipc/
│   ├── mod.rs       Unix socket listener
│   ├── schema.rs    NDJSON request/response types
│   ├── handler.rs   Method dispatch → HAT drivers
│   └── params.rs    Typed IPC parameter extraction
├── hat/
│   ├── mod.rs       HAT abstraction
│   ├── i2c.rs       I2C read/write helpers
│   ├── pwm.rs       PWM register protocol
│   ├── adc.rs       ADC reads
│   ├── servo.rs     Servo control + TTL watchdog
│   ├── motor.rs     DC motor speed/direction control
│   ├── battery.rs   Battery voltage
│   ├── gpio.rs      Named GPIO pins
│   ├── ultrasonic.rs HC-SR04 distance sensor
│   └── audio.rs     ALSA volume/gain control
├── calibration.rs   Motor/servo/grayscale calibration store
├── routine/
│   ├── mod.rs       Routine engine (start/stop/status)
│   └── explore.rs   Autonomous explore routine
├── reset.rs         MCU reset (BCM5)
└── testing.rs       Shared test mocks (MockI2c, MockGpio, MockAlsaControl)
```

## Wi-Fi Soft AP Pairing

When the device cannot reach a known Wi-Fi network, the Soft AP watchdog
(`systemd/nomon-softap-watchdog.timer`) calls `scripts/ap-mode.sh up` to
broadcast a WPA2 hotspot named `nomon-<last4-of-MAC>`. The passphrase is read
from `/var/lib/nomon/ap_passphrase` — a long random secret that nomothetic
generates on first boot, separate from the 8-digit pairing code (an 8-digit
WPA2 PSK would be crackable offline from a captured handshake).

The hotspot is accessible from any browser or the nomotactic app:

```
SSID:       nomon-<last4-of-MAC>
Passphrase: contents of /var/lib/nomon/ap_passphrase
            (also shown at /run/nomothetic/ap-passphrase on the Pi)
Device IP:  192.168.4.1
API:        http://192.168.4.1:8080
```

Once connected, open `http://192.168.4.1:8080` and follow the on-screen
pairing prompt to obtain a device-scoped JWT. Being on the AP network is the
proof of possession; the 8-digit pairing code is only needed when pairing over
the home network instead.

The watchdog automatically deactivates the AP once the Pi acquires a full
internet connection, restoring normal operation.

See [`docs/adr/005-wifi-soft-ap.md`](docs/adr/005-wifi-soft-ap.md) for design
rationale and [`docs/architecture.md`](docs/architecture.md) for the Soft AP
architecture section.

## Documentation

- [Architecture](docs/architecture.md) — system design, data flows, concurrency model
- [Roadmap](docs/roadmap.md) — development phases and progress tracking
- [Hardware Reference](docs/hardware_reference.md) — Robot HAT V4 register map
- [Contributing](CONTRIBUTING.md) — development setup and code guidelines

## Related

- [nomothetic](https://github.com/Perceptua/nomothetic) — Python package (camera, API, telemetry, HAT IPC client)
- [IPC Schema](https://github.com/Perceptua/nomothetic/blob/main/docs/hat_ipc_schema.md) — full protocol specification

## License

MIT
