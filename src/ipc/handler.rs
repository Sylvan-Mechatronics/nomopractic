// Request dispatch — routes method names to HAT driver functions.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;
use tracing::{debug, error, warn};

use super::params::ParamExtractor;
use super::schema::{Request, Response};
use crate::calibration::CalibrationStore;
use crate::config::Config;
use crate::hat::adc;
use crate::hat::audio::{AlsaControl, AmixerControl, AudioError};
use crate::hat::battery;
use crate::hat::gpio::{self, GpioError, HatGpio};
use crate::hat::i2c::{Hat, HatError};
use crate::hat::motor::{self, MotorError};
use crate::hat::servo::LeaseManager;
use crate::hat::ultrasonic;
use crate::hat::{pwm, servo};
use crate::reset;
use crate::routine::RoutineEngine;

/// Minimum interval between consecutive MCU reset requests (ms).
const RESET_MIN_INTERVAL_MS: u64 = 1000;

/// Snapshot of MCU reset state, kept under a single mutex for consistency.
struct McuState {
    reset_count: u64,
    last_reset_at: Option<Instant>,
}

/// Maps a domain error to an IPC error code string.
trait ErrorCode {
    fn error_code(&self) -> &'static str;
}

impl ErrorCode for HatError {
    fn error_code(&self) -> &'static str {
        match self {
            HatError::I2c(_) => "HARDWARE_ERROR",
            HatError::InvalidChannel(_)
            | HatError::InvalidServoChannel(_)
            | HatError::InvalidMotorChannel(_)
            | HatError::InvalidPulse(_)
            | HatError::InvalidAngle(_)
            | HatError::InvalidParam(_) => "INVALID_PARAMS",
        }
    }
}

impl ErrorCode for MotorError {
    fn error_code(&self) -> &'static str {
        match self {
            MotorError::Hat(he) => he.error_code(),
            MotorError::Gpio(ge) => ge.error_code(),
        }
    }
}

impl ErrorCode for GpioError {
    fn error_code(&self) -> &'static str {
        match self {
            GpioError::Gpio(_) => "HARDWARE_ERROR",
            GpioError::ReadOnly(_) => "INVALID_PARAMS",
        }
    }
}

impl ErrorCode for AudioError {
    fn error_code(&self) -> &'static str {
        match self {
            AudioError::Command(_) | AudioError::Io(_) => "HARDWARE_ERROR",
            AudioError::Parse(_) => "INTERNAL_ERROR",
        }
    }
}

/// Processes incoming IPC requests and returns serialized JSON responses.
pub struct Handler {
    config: Arc<Config>,
    start_time: Instant,
    hat: Arc<Hat>,
    lease_manager: Arc<LeaseManager>,
    motor_lease_manager: Arc<LeaseManager>,
    gpio: Arc<HatGpio>,
    mcu_state: tokio::sync::Mutex<McuState>,
    alsa: Arc<dyn AlsaControl>,
    calibration: Arc<tokio::sync::Mutex<CalibrationStore>>,
    routine: Arc<tokio::sync::Mutex<RoutineEngine>>,
}

impl Handler {
    pub fn new(config: Arc<Config>, hat: Arc<Hat>, gpio: Arc<HatGpio>) -> Self {
        let alsa: Arc<dyn AlsaControl> = Arc::new(AmixerControl {
            output_card_index: config.audio.output_card_index,
            output_card_name: config.audio.output_card_name.clone(),
            output_control: config.audio.output_control.clone(),
            input_card_index: config.audio.input_card_index,
            input_card_name: config.audio.input_card_name.clone(),
            input_controls: config.audio.input_controls.clone(),
        });
        Self::with_alsa(config, hat, gpio, alsa)
    }

    /// Like `new` but accepts an explicit `AlsaControl` implementation.
    ///
    /// Used by tests to inject a `MockAlsaControl` without touching real ALSA.
    pub fn with_alsa(
        config: Arc<Config>,
        hat: Arc<Hat>,
        gpio: Arc<HatGpio>,
        alsa: Arc<dyn AlsaControl>,
    ) -> Self {
        let n_motors = config.motors.len();
        let calibration_store =
            CalibrationStore::load_or_default(&config.calibration_path, n_motors);
        let calibration = Arc::new(tokio::sync::Mutex::new(calibration_store));
        let motor_lease_manager = Arc::new(LeaseManager::new());
        let routine = Arc::new(tokio::sync::Mutex::new(RoutineEngine::new(
            hat.clone(),
            gpio.clone(),
            config.clone(),
            calibration.clone(),
            motor_lease_manager.clone(),
        )));
        Self {
            config,
            start_time: Instant::now(),
            hat,
            lease_manager: Arc::new(LeaseManager::new()),
            motor_lease_manager,
            gpio,
            mcu_state: tokio::sync::Mutex::new(McuState {
                reset_count: 0,
                last_reset_at: None,
            }),
            alsa,
            calibration,
            routine,
        }
    }

    pub fn lease_manager(&self) -> &Arc<LeaseManager> {
        &self.lease_manager
    }

    pub fn motor_lease_manager(&self) -> &Arc<LeaseManager> {
        &self.motor_lease_manager
    }

    pub fn hat(&self) -> &Arc<Hat> {
        &self.hat
    }

    pub fn config(&self) -> &Arc<Config> {
        &self.config
    }

    /// Release all servo and motor leases for a disconnected connection and idle
    /// any active channels.
    pub async fn on_client_disconnect(&self, conn_id: u64) {
        // Idle servo channels.
        let servo_channels = self.lease_manager.release_connection(conn_id).await;
        for ch in servo_channels {
            warn!(
                channel = ch,
                conn_id, "client disconnected; idling leased servo channel"
            );
            if let Err(e) = pwm::set_channel_pulse_us(&self.hat, ch, 0).await {
                error!(error = %e, channel = ch, "failed to idle servo channel on client disconnect");
            }
        }
        // Idle motor channels.
        let motor_channels = self.motor_lease_manager.release_connection(conn_id).await;
        for ipc_ch in motor_channels {
            warn!(
                channel = ipc_ch,
                conn_id, "client disconnected; stopping leased motor"
            );
            if let Some(cfg) = self.config.motors.get(ipc_ch as usize)
                && let Err(e) = motor::idle_motor(&self.hat, cfg.pwm_channel).await
            {
                error!(error = %e, channel = ipc_ch, "failed to stop motor on client disconnect");
            }
        }
    }

    /// Parse a raw JSON line, dispatch the method, and return a JSON response string.
    ///
    /// `conn_id` identifies the originating connection for servo lease tracking.
    pub async fn dispatch(&self, raw: &str, conn_id: u64) -> String {
        let request: Request = match serde_json::from_str(raw) {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, "malformed request");
                let resp = Response::err(
                    String::new(),
                    "INVALID_PARAMS",
                    format!("malformed JSON: {e}"),
                );
                return serde_json::to_string(&resp)
                    .unwrap_or_else(|_| r#"{"id":"","ok":false,"error":{"code":"INTERNAL_ERROR","message":"serialization failed"}}"#.into());
            }
        };

        debug!(id = %request.id, method = %request.method, "dispatching");

        let response = match request.method.as_str() {
            "health" => self.handle_health(&request),
            "get_battery_voltage" => self.handle_get_battery_voltage(&request).await,
            "read_adc" => self.handle_read_adc(&request).await,
            "set_servo_pulse_us" => self.handle_set_servo_pulse_us(&request, conn_id).await,
            "set_servo_angle" => self.handle_set_servo_angle(&request, conn_id).await,
            "read_gpio" => self.handle_read_gpio(&request).await,
            "write_gpio" => self.handle_write_gpio(&request).await,
            "reset_mcu" => self.handle_reset_mcu(&request).await,
            "set_motor_speed" => self.handle_set_motor_speed(&request, conn_id).await,
            "stop_all_motors" => self.handle_stop_all_motors(&request).await,
            "get_motor_status" => self.handle_get_motor_status(&request).await,
            "get_servo_status" => self.handle_get_servo_status(&request).await,
            "get_mcu_status" => self.handle_get_mcu_status(&request).await,
            // Convenience / coordinated methods.
            "drive" => self.handle_drive(&request, conn_id).await,
            "steer" => self.handle_steer(&request, conn_id).await,
            "pan_camera" => self.handle_pan_camera(&request, conn_id).await,
            "tilt_camera" => self.handle_tilt_camera(&request, conn_id).await,
            "read_grayscale" => self.handle_read_grayscale(&request).await,
            "read_ultrasonic" => self.handle_read_ultrasonic(&request).await,
            "enable_speaker" => self.handle_enable_speaker(&request).await,
            "disable_speaker" => self.handle_disable_speaker(&request).await,
            "set_volume" => self.handle_set_volume(&request).await,
            "get_volume" => self.handle_get_volume(&request).await,
            "set_mic_gain" => self.handle_set_mic_gain(&request).await,
            "get_mic_gain" => self.handle_get_mic_gain(&request).await,
            // Calibration methods.
            "get_calibration" => self.handle_get_calibration(&request).await,
            "set_motor_calibration" => self.handle_set_motor_calibration(&request).await,
            "set_servo_calibration" => self.handle_set_servo_calibration(&request).await,
            "calibrate_grayscale" => self.handle_calibrate_grayscale(&request).await,
            "read_grayscale_normalized" => self.handle_read_grayscale_normalized(&request).await,
            "save_calibration" => self.handle_save_calibration(&request).await,
            "reset_calibration" => self.handle_reset_calibration(&request).await,
            // Routine methods.
            "start_routine" => self.handle_start_routine(&request).await,
            "stop_routine" => self.handle_stop_routine(&request).await,
            "get_routine_status" => self.handle_get_routine_status(&request).await,
            other => {
                warn!(method = other, "unknown method");
                Response::err(
                    request.id,
                    "UNKNOWN_METHOD",
                    format!("method '{}' not recognized", other),
                )
            }
        };

        serde_json::to_string(&response)
            .unwrap_or_else(|_| r#"{"id":"","ok":false,"error":{"code":"INTERNAL_ERROR","message":"serialization failed"}}"#.into())
    }

    fn handle_health(&self, request: &Request) -> Response {
        let uptime = self.start_time.elapsed();
        Response::ok(
            request.id.clone(),
            json!({
                "schema_version": "1.0.0",
                "status": "ok",
                "version": env!("CARGO_PKG_VERSION"),
                "hat_address": format!("0x{:02x}", self.config.hat_address),
                "i2c_bus": self.config.i2c_bus,
                "uptime_s": uptime.as_secs(),
            }),
        )
    }

    async fn handle_get_battery_voltage(&self, request: &Request) -> Response {
        match battery::get_battery_voltage(&self.hat).await {
            Ok(voltage) => Response::ok(request.id.clone(), json!({ "voltage_v": voltage })),
            Err(e) => {
                warn!(error = %e, "battery read failed");
                Response::err(request.id.clone(), "HARDWARE_ERROR", e.to_string())
            }
        }
    }

    async fn handle_read_adc(&self, request: &Request) -> Response {
        let ext = ParamExtractor::new(request);
        let channel =
            match ext.required_u64_as_u8("channel", 0, 7, "channel is required and must be 0–7") {
                Ok(ch) => ch,
                Err(resp) => return resp,
            };
        match adc::read_adc(&self.hat, channel).await {
            Ok(raw) => Response::ok(
                request.id.clone(),
                json!({ "channel": channel, "raw_value": raw }),
            ),
            Err(e) => {
                warn!(error = %e, "read_adc failed");
                Response::err(request.id.clone(), e.error_code(), e.to_string())
            }
        }
    }

    async fn handle_set_servo_pulse_us(&self, request: &Request, conn_id: u64) -> Response {
        let channel = match extract_channel(request) {
            Ok(ch) => ch,
            Err(resp) => return resp,
        };
        let pulse_us = match request.params.get("pulse_us").and_then(|v| v.as_u64()) {
            Some(p) if (500..=2500).contains(&p) => p as u16,
            Some(_) | None => {
                return Response::err(
                    request.id.clone(),
                    "INVALID_PARAMS",
                    "pulse_us is required and must be 500–2500",
                );
            }
        };
        let ttl_ms = extract_ttl(request, self.config.servo_default_ttl_ms);
        let ttl_ms = match ttl_ms {
            Ok(ms) => ms,
            Err(resp) => return resp,
        };

        match servo::set_servo_pulse_us(&self.hat, channel, pulse_us).await {
            Ok(()) => {
                self.lease_manager.set_lease(channel, conn_id, ttl_ms).await;
                Response::ok(
                    request.id.clone(),
                    json!({ "channel": channel, "pulse_us": pulse_us }),
                )
            }
            Err(e) => {
                warn!(error = %e, "set_servo_pulse_us failed");
                Response::err(request.id.clone(), e.error_code(), e.to_string())
            }
        }
    }

    async fn handle_set_servo_angle(&self, request: &Request, conn_id: u64) -> Response {
        let channel = match extract_channel(request) {
            Ok(ch) => ch,
            Err(resp) => return resp,
        };
        let ext = ParamExtractor::new(request);
        let angle_deg = match ext.required_f64(
            "angle_deg",
            0.0,
            180.0,
            "angle_deg is required and must be 0.0–180.0",
        ) {
            Ok(a) => a,
            Err(resp) => return resp,
        };
        let ttl_ms = extract_ttl(request, self.config.servo_default_ttl_ms);
        let ttl_ms = match ttl_ms {
            Ok(ms) => ms,
            Err(resp) => return resp,
        };

        match servo::set_servo_angle(&self.hat, channel, angle_deg).await {
            Ok(pulse_us) => {
                self.lease_manager.set_lease(channel, conn_id, ttl_ms).await;
                Response::ok(
                    request.id.clone(),
                    json!({
                        "channel": channel,
                        "angle_deg": angle_deg,
                        "pulse_us": pulse_us,
                    }),
                )
            }
            Err(e) => {
                warn!(error = %e, "set_servo_angle failed");
                Response::err(request.id.clone(), e.error_code(), e.to_string())
            }
        }
    }

    async fn handle_read_gpio(&self, request: &Request) -> Response {
        let ext = ParamExtractor::new(request);
        let pin_name = match ext.required_str("pin", "pin is required") {
            Ok(s) => s,
            Err(resp) => return resp,
        };
        let pin = match gpio::GpioPin::from_name(&pin_name) {
            Some(p) => p,
            None => {
                return Response::err(
                    request.id.clone(),
                    "INVALID_PARAMS",
                    format!(
                        "unknown pin '{pin_name}'; valid: D2, D3, D4, D5, MCURST, SW, LED, SPEAKER_EN"
                    ),
                );
            }
        };
        match gpio::read_gpio_pin(&self.gpio, pin).await {
            Ok(high) => Response::ok(
                request.id.clone(),
                json!({ "pin": pin.name(), "high": high }),
            ),
            Err(e) => {
                warn!(error = %e, "read_gpio failed");
                Response::err(request.id.clone(), e.error_code(), e.to_string())
            }
        }
    }

    async fn handle_write_gpio(&self, request: &Request) -> Response {
        let ext = ParamExtractor::new(request);
        let pin_name = match ext.required_str("pin", "pin is required") {
            Ok(s) => s,
            Err(resp) => return resp,
        };
        let pin = match gpio::GpioPin::from_name(&pin_name) {
            Some(p) => p,
            None => {
                return Response::err(
                    request.id.clone(),
                    "INVALID_PARAMS",
                    format!(
                        "unknown pin '{pin_name}'; valid output pins: D2, D4, D5, MCURST, LED, SPEAKER_EN"
                    ),
                );
            }
        };
        let high = match ext.required_bool("high", "high is required and must be a boolean") {
            Ok(h) => h,
            Err(resp) => return resp,
        };
        match gpio::write_gpio_pin(&self.gpio, pin, high).await {
            Ok(()) => Response::ok(
                request.id.clone(),
                json!({ "pin": pin.name(), "high": high }),
            ),
            Err(e) => {
                warn!(error = %e, "write_gpio failed");
                Response::err(request.id.clone(), e.error_code(), e.to_string())
            }
        }
    }

    async fn handle_reset_mcu(&self, request: &Request) -> Response {
        let now = Instant::now();
        {
            // Check the rate-limit and release the lock before the reset so
            // that `reset_mcu` (which acquires `gpio.bus`) is not executed
            // under `mcu_state` — this avoids holding the lock across an
            // async sleep.  The lock is re-acquired below only on success;
            // a failed attempt therefore does not block a retry.
            let guard = self.mcu_state.lock().await;
            if let Some(last) = guard.last_reset_at {
                let elapsed_ms = now
                    .checked_duration_since(last)
                    .unwrap_or(Duration::ZERO)
                    .as_millis() as u64;
                if elapsed_ms < RESET_MIN_INTERVAL_MS {
                    let remaining = RESET_MIN_INTERVAL_MS - elapsed_ms;
                    return Response::err(
                        request.id.clone(),
                        "HARDWARE_ERROR",
                        format!("MCU reset rate-limited; retry after {remaining} ms"),
                    );
                }
            }
        }
        match reset::reset_mcu(&self.gpio).await {
            Ok(result) => {
                let mut guard = self.mcu_state.lock().await;
                guard.last_reset_at = Some(now);
                guard.reset_count += 1;
                Response::ok(request.id.clone(), json!({ "reset_ms": result.reset_ms }))
            }
            Err(e) => {
                warn!(error = %e, "reset_mcu failed");
                Response::err(request.id.clone(), "HARDWARE_ERROR", e.to_string())
            }
        }
    }
    async fn handle_get_servo_status(&self, request: &Request) -> Response {
        let leases = self.lease_manager.get_active_leases().await;
        let active: Vec<_> = leases
            .iter()
            .map(|(ch, ttl_remaining_ms, conn_id)| {
                json!({
                    "channel": ch,
                    "ttl_remaining_ms": ttl_remaining_ms,
                    "conn_id": conn_id,
                })
            })
            .collect();
        Response::ok(request.id.clone(), json!({ "active_leases": active }))
    }

    async fn handle_get_mcu_status(&self, request: &Request) -> Response {
        let guard = self.mcu_state.lock().await;
        let resets = guard.reset_count;
        let last_reset_s_ago = guard.last_reset_at.map(|t| t.elapsed().as_secs());
        Response::ok(
            request.id.clone(),
            json!({
                "resets_since_start": resets,
                "last_reset_s_ago": last_reset_s_ago,
            }),
        )
    }

    /// Apply calibration to a speed value for a specific motor channel.
    ///
    /// Returns `(effective_speed, reversed)` — the calibration-adjusted speed
    /// and the final direction-reversal flag.
    async fn calibrated_speed(&self, ipc_channel: u8, speed_pct: f64) -> (f64, bool) {
        let cal_guard = self.calibration.lock().await;
        cal_guard.motors[ipc_channel as usize]
            .apply(speed_pct, self.config.motors[ipc_channel as usize].reversed)
    }

    /// Command one or more motors atomically with rollback on partial failure.
    ///
    /// `motor_speeds` maps IPC channel → desired speed percentage.  Calibration
    /// is applied per-channel.  On success, leases are committed.  On partial
    /// failure, already-set motors are rolled back (idled) and no leases are
    /// committed.
    ///
    /// Returns `Ok(succeeded_channels)` or `Err(error_messages)`.
    async fn atomic_motor_operation(
        &self,
        motor_speeds: &[(u8, f64)],
        conn_id: u64,
        ttl_ms: u64,
    ) -> Result<Vec<u8>, Vec<String>> {
        let mut succeeded: Vec<u8> = Vec::new();
        let mut errors: Vec<String> = Vec::new();

        for &(ipc_ch, speed_pct) in motor_speeds {
            let (effective_speed, final_reversed) = self.calibrated_speed(ipc_ch, speed_pct).await;
            let cfg = &self.config.motors[ipc_ch as usize];
            match motor::set_motor_speed(
                &self.hat,
                &self.gpio,
                cfg.pwm_channel,
                cfg.dir_pin_bcm,
                final_reversed,
                effective_speed,
            )
            .await
            {
                Ok(()) => succeeded.push(ipc_ch),
                Err(e) => {
                    warn!(error = %e, channel = ipc_ch, "motor command failed");
                    errors.push(format!("channel {ipc_ch}: {e}"));
                }
            }
        }

        if errors.is_empty() {
            for &ipc_ch in &succeeded {
                self.motor_lease_manager
                    .set_lease(ipc_ch, conn_id, ttl_ms)
                    .await;
            }
            Ok(succeeded)
        } else {
            // Rollback: idle any motors that did start.
            for &ipc_ch in &succeeded {
                let cfg = &self.config.motors[ipc_ch as usize];
                if let Err(e) = motor::idle_motor(&self.hat, cfg.pwm_channel).await {
                    error!(error = %e, channel = ipc_ch, "rollback idle failed");
                    errors.push(format!("channel {ipc_ch}: rollback failed: {e}"));
                }
            }
            Err(errors)
        }
    }

    async fn handle_set_motor_speed(&self, request: &Request, conn_id: u64) -> Response {
        let num_motors = self.config.motors.len();
        let ipc_channel = match request.params.get("channel").and_then(|v| v.as_u64()) {
            Some(ch) if (ch as usize) < num_motors => ch as u8,
            Some(ch) => {
                return Response::err(
                    request.id.clone(),
                    "INVALID_PARAMS",
                    format!("channel {ch} is not configured; {num_motors} motor(s) available"),
                );
            }
            None => {
                return Response::err(request.id.clone(), "INVALID_PARAMS", "channel is required");
            }
        };
        let speed_pct = match request.params.get("speed_pct").and_then(|v| v.as_f64()) {
            Some(s) if (-100.0..=100.0).contains(&s) => s,
            Some(s) => {
                return Response::err(
                    request.id.clone(),
                    "INVALID_PARAMS",
                    format!("speed_pct {s} is out of range -100.0..=100.0"),
                );
            }
            None => {
                return Response::err(
                    request.id.clone(),
                    "INVALID_PARAMS",
                    "speed_pct is required",
                );
            }
        };
        let ttl_ms = match extract_ttl(request, self.config.motor_default_ttl_ms) {
            Ok(ms) => ms,
            Err(resp) => return resp,
        };

        match self
            .atomic_motor_operation(&[(ipc_channel, speed_pct)], conn_id, ttl_ms)
            .await
        {
            Ok(_) => Response::ok(
                request.id.clone(),
                json!({ "channel": ipc_channel, "speed_pct": speed_pct }),
            ),
            Err(errors) => Response::err(request.id.clone(), "HARDWARE_ERROR", errors.join("; ")),
        }
    }

    async fn handle_stop_all_motors(&self, request: &Request) -> Response {
        let mut errors: Vec<String> = Vec::new();
        for (ipc_ch, cfg) in self.config.motors.iter().enumerate() {
            if let Err(e) = motor::idle_motor(&self.hat, cfg.pwm_channel).await {
                error!(error = %e, channel = ipc_ch, "failed to stop motor");
                errors.push(format!("channel {ipc_ch}: {e}"));
            } else {
                self.motor_lease_manager.revoke_channel(ipc_ch as u8).await;
            }
        }
        if errors.is_empty() {
            Response::ok(
                request.id.clone(),
                json!({ "stopped": self.config.motors.len() }),
            )
        } else {
            Response::err(request.id.clone(), "HARDWARE_ERROR", errors.join("; "))
        }
    }

    async fn handle_get_motor_status(&self, request: &Request) -> Response {
        let leases = self.motor_lease_manager.get_active_leases().await;
        let active: Vec<_> = leases
            .iter()
            .map(|(ch, ttl_remaining_ms, conn_id)| {
                json!({
                    "channel": ch,
                    "ttl_remaining_ms": ttl_remaining_ms,
                    "conn_id": conn_id,
                })
            })
            .collect();
        Response::ok(request.id.clone(), json!({ "active_leases": active }))
    }

    // -----------------------------------------------------------------------
    // Convenience / coordinated methods
    // -----------------------------------------------------------------------

    /// `drive { speed_pct, ttl_ms? }` — set both configured motors to the
    /// same speed simultaneously.  All configured motors are commanded in a
    /// single Rust call to avoid the per-motor latency and race conditions that
    /// would occur when issuing two separate `set_motor_speed` IPC requests.
    ///
    /// Failure-atomic: leases are only committed after **all** motors have been
    /// successfully commanded.  If any motor fails, already-set motors are
    /// rolled back (idled) and no leases are committed.
    async fn handle_drive(&self, request: &Request, conn_id: u64) -> Response {
        let ext = ParamExtractor::new(request);
        let speed_pct = match ext.required_f64(
            "speed_pct",
            -100.0,
            100.0,
            "speed_pct is required and must be -100.0–100.0",
        ) {
            Ok(s) => s,
            Err(resp) => return resp,
        };
        let ttl_ms = match extract_ttl(request, self.config.motor_default_ttl_ms) {
            Ok(ms) => ms,
            Err(resp) => return resp,
        };

        if self.config.motors.is_empty() {
            return Response::err(request.id.clone(), "INVALID_PARAMS", "no motors configured");
        }

        let motor_speeds: Vec<(u8, f64)> = (0..self.config.motors.len())
            .map(|i| (i as u8, speed_pct))
            .collect();

        match self
            .atomic_motor_operation(&motor_speeds, conn_id, ttl_ms)
            .await
        {
            Ok(_) => Response::ok(
                request.id.clone(),
                json!({
                    "speed_pct": speed_pct,
                    "motors": self.config.motors.len(),
                }),
            ),
            Err(errors) => Response::err(request.id.clone(), "HARDWARE_ERROR", errors.join("; ")),
        }
    }

    /// `steer { angle_deg, ttl_ms? }` — set the steering servo by name.
    async fn handle_steer(&self, request: &Request, conn_id: u64) -> Response {
        let ch = match self.config.servos.steering {
            Some(ch) => ch,
            None => {
                return Response::err(
                    request.id.clone(),
                    "INVALID_PARAMS",
                    "steering servo is not configured",
                );
            }
        };
        self.set_named_servo(request, conn_id, ch, "steering").await
    }

    /// `pan_camera { angle_deg, ttl_ms? }` — set the camera pan servo by name.
    async fn handle_pan_camera(&self, request: &Request, conn_id: u64) -> Response {
        let ch = match self.config.servos.camera_pan {
            Some(ch) => ch,
            None => {
                return Response::err(
                    request.id.clone(),
                    "INVALID_PARAMS",
                    "camera_pan servo is not configured",
                );
            }
        };
        self.set_named_servo(request, conn_id, ch, "camera_pan")
            .await
    }

    /// `tilt_camera { angle_deg, ttl_ms? }` — set the camera tilt servo by name.
    async fn handle_tilt_camera(&self, request: &Request, conn_id: u64) -> Response {
        let ch = match self.config.servos.camera_tilt {
            Some(ch) => ch,
            None => {
                return Response::err(
                    request.id.clone(),
                    "INVALID_PARAMS",
                    "camera_tilt servo is not configured",
                );
            }
        };
        self.set_named_servo(request, conn_id, ch, "camera_tilt")
            .await
    }

    /// Common implementation for named-servo angle-set methods.
    async fn set_named_servo(
        &self,
        request: &Request,
        conn_id: u64,
        channel: u8,
        servo_name: &str,
    ) -> Response {
        let angle_deg = match request.params.get("angle_deg").and_then(|v| v.as_f64()) {
            Some(a) if (0.0..=180.0).contains(&a) => a,
            Some(_) | None => {
                return Response::err(
                    request.id.clone(),
                    "INVALID_PARAMS",
                    "angle_deg is required and must be 0.0–180.0",
                );
            }
        };
        let ttl_ms = match extract_ttl(request, self.config.servo_default_ttl_ms) {
            Ok(ms) => ms,
            Err(resp) => return resp,
        };

        // Acquire calibration trim, copy value, drop guard before hardware call.
        let trim_us: i16 = {
            let cal_guard = self.calibration.lock().await;
            cal_guard
                .servos
                .get(servo_name)
                .map(|s| s.trim_us)
                .unwrap_or(0)
        };

        let effective_pulse = servo::angle_to_trimmed_pulse_us(angle_deg, trim_us);

        match servo::set_servo_pulse_us(&self.hat, channel, effective_pulse).await {
            Ok(()) => {
                self.lease_manager.set_lease(channel, conn_id, ttl_ms).await;
                Response::ok(
                    request.id.clone(),
                    json!({
                        "servo": servo_name,
                        "channel": channel,
                        "angle_deg": angle_deg,
                        "pulse_us": effective_pulse,
                    }),
                )
            }
            Err(e) => {
                warn!(error = %e, servo = servo_name, "set_named_servo failed");
                Response::err(request.id.clone(), e.error_code(), e.to_string())
            }
        }
    }

    /// `read_grayscale {}` — read all three grayscale sensor ADC channels and
    /// return raw values as `[left, center, right]`.
    async fn handle_read_grayscale(&self, request: &Request) -> Response {
        let [ch0, ch1, ch2] = self.config.sensors.grayscale;
        let mut values = [0u16; 3];
        for (i, ch) in [ch0, ch1, ch2].iter().enumerate() {
            match adc::read_adc(&self.hat, *ch).await {
                Ok(raw) => values[i] = raw,
                Err(e) => {
                    warn!(error = %e, channel = ch, "read_grayscale ADC read failed");
                    return Response::err(
                        request.id.clone(),
                        e.error_code(),
                        format!("grayscale channel {ch}: {e}"),
                    );
                }
            }
        }
        Response::ok(
            request.id.clone(),
            json!({
                "channels": [ch0, ch1, ch2],
                "values": [values[0], values[1], values[2]],
            }),
        )
    }

    /// `read_ultrasonic {}` — trigger the HC-SR04 sensor and return distance in
    /// centimetres. Uses the TRIG/ECHO BCM pins and timeout from config.
    async fn handle_read_ultrasonic(&self, request: &Request) -> Response {
        let cfg = &self.config.ultrasonic;
        match ultrasonic::read_distance_cm(
            &self.gpio,
            cfg.trig_pin_bcm,
            cfg.echo_pin_bcm,
            cfg.timeout_ms,
        )
        .await
        {
            Ok(distance_cm) => {
                Response::ok(request.id.clone(), json!({ "distance_cm": distance_cm }))
            }
            Err(ultrasonic::UltrasonicError::Timeout(ms)) => Response::err(
                request.id.clone(),
                "TIMEOUT",
                format!("ultrasonic measurement timed out after {ms} ms"),
            ),
            Err(ultrasonic::UltrasonicError::NoEcho) => Response::err(
                request.id.clone(),
                "NO_ECHO",
                "no valid echo received (object out of sensor range 2–400 cm)",
            ),
            Err(ultrasonic::UltrasonicError::Gpio(e)) => {
                Response::err(request.id.clone(), e.error_code(), e.to_string())
            }
        }
    }

    /// `enable_speaker {}` — assert the speaker-amplifier enable pin HIGH.
    async fn handle_enable_speaker(&self, request: &Request) -> Response {
        let bcm = self.config.speaker_en_pin_bcm;
        match gpio::write_gpio_bcm(&self.gpio, bcm, true).await {
            Ok(()) => Response::ok(
                request.id.clone(),
                json!({ "enabled": true, "pin_bcm": bcm }),
            ),
            Err(e) => Response::err(request.id.clone(), e.error_code(), e.to_string()),
        }
    }

    /// `disable_speaker {}` — pull the speaker-amplifier enable pin LOW.
    async fn handle_disable_speaker(&self, request: &Request) -> Response {
        let bcm = self.config.speaker_en_pin_bcm;
        match gpio::write_gpio_bcm(&self.gpio, bcm, false).await {
            Ok(()) => Response::ok(
                request.id.clone(),
                json!({ "enabled": false, "pin_bcm": bcm }),
            ),
            Err(e) => Response::err(request.id.clone(), e.error_code(), e.to_string()),
        }
    }

    /// `set_volume { volume_pct: u8 }` — set output volume via ALSA mixer.
    async fn handle_set_volume(&self, request: &Request) -> Response {
        let ext = ParamExtractor::new(request);
        let volume_pct = match ext.required_u64_as_u8(
            "volume_pct",
            0,
            100,
            "volume_pct is required and must be 0–100",
        ) {
            Ok(v) => v,
            Err(resp) => return resp,
        };
        let alsa = Arc::clone(&self.alsa);
        match tokio::task::spawn_blocking(move || alsa.set_volume_pct(volume_pct)).await {
            Ok(Ok(())) => {
                tracing::info!(volume_pct, "output volume set");
                Response::ok(request.id.clone(), json!({ "volume_pct": volume_pct }))
            }
            Ok(Err(e)) => {
                warn!(error = %e, "set_volume failed");
                Response::err(request.id.clone(), e.error_code(), e.to_string())
            }
            Err(e) => Response::err(request.id.clone(), "INTERNAL_ERROR", e.to_string()),
        }
    }

    /// `get_volume {}` — read current output volume from ALSA mixer.
    async fn handle_get_volume(&self, request: &Request) -> Response {
        let alsa = Arc::clone(&self.alsa);
        match tokio::task::spawn_blocking(move || alsa.get_volume_pct()).await {
            Ok(Ok(pct)) => Response::ok(request.id.clone(), json!({ "volume_pct": pct })),
            Ok(Err(e)) => {
                warn!(error = %e, "get_volume failed");
                Response::err(request.id.clone(), e.error_code(), e.to_string())
            }
            Err(e) => Response::err(request.id.clone(), "INTERNAL_ERROR", e.to_string()),
        }
    }

    /// `set_mic_gain { gain_pct: u8 }` — set microphone capture gain via ALSA.
    async fn handle_set_mic_gain(&self, request: &Request) -> Response {
        let ext = ParamExtractor::new(request);
        let gain_pct = match ext.required_u64_as_u8(
            "gain_pct",
            0,
            100,
            "gain_pct is required and must be 0–100",
        ) {
            Ok(v) => v,
            Err(resp) => return resp,
        };
        let alsa = Arc::clone(&self.alsa);
        match tokio::task::spawn_blocking(move || alsa.set_mic_gain_pct(gain_pct)).await {
            Ok(Ok(())) => {
                tracing::info!(gain_pct, "microphone gain set");
                Response::ok(request.id.clone(), json!({ "gain_pct": gain_pct }))
            }
            Ok(Err(e)) => {
                warn!(error = %e, "set_mic_gain failed");
                Response::err(request.id.clone(), e.error_code(), e.to_string())
            }
            Err(e) => Response::err(request.id.clone(), "INTERNAL_ERROR", e.to_string()),
        }
    }

    /// `get_mic_gain {}` — read current microphone capture gain from ALSA.
    async fn handle_get_mic_gain(&self, request: &Request) -> Response {
        let alsa = Arc::clone(&self.alsa);
        match tokio::task::spawn_blocking(move || alsa.get_mic_gain_pct()).await {
            Ok(Ok(pct)) => Response::ok(request.id.clone(), json!({ "gain_pct": pct })),
            Ok(Err(e)) => {
                warn!(error = %e, "get_mic_gain failed");
                Response::err(request.id.clone(), e.error_code(), e.to_string())
            }
            Err(e) => Response::err(request.id.clone(), "INTERNAL_ERROR", e.to_string()),
        }
    }

    // -----------------------------------------------------------------------
    // Calibration methods
    // -----------------------------------------------------------------------

    /// `get_calibration {}` — return a full snapshot of the in-memory
    /// calibration store, merging `adc_channel` from `config.sensors.grayscale`.
    async fn handle_get_calibration(&self, request: &Request) -> Response {
        let guard = self.calibration.lock().await;
        let store = guard.clone();
        drop(guard);

        let motors: Vec<serde_json::Value> = store
            .motors
            .iter()
            .enumerate()
            .map(|(i, m)| {
                json!({
                    "channel": i,
                    "speed_scale": m.speed_scale,
                    "deadband_pct": m.deadband_pct,
                    "reversed": m.reversed,
                })
            })
            .collect();

        let [adc0, adc1, adc2] = self.config.sensors.grayscale;
        let grayscale: Vec<serde_json::Value> = store
            .grayscale
            .iter()
            .zip([adc0, adc1, adc2])
            .map(|(g, adc_ch)| {
                json!({
                    "adc_channel": adc_ch,
                    "white_raw": g.white_raw,
                    "black_raw": g.black_raw,
                })
            })
            .collect();

        let servos: serde_json::Map<String, serde_json::Value> = store
            .servos
            .iter()
            .map(|(name, s)| (name.clone(), json!({ "trim_us": s.trim_us })))
            .collect();

        Response::ok(
            request.id.clone(),
            json!({
                "motors": motors,
                "servos": servos,
                "grayscale": grayscale,
            }),
        )
    }

    /// `set_motor_calibration { channel, speed_scale?, deadband_pct?, reversed? }` —
    /// partial update of motor calibration; unspecified fields are unchanged.
    async fn handle_set_motor_calibration(&self, request: &Request) -> Response {
        let n_motors = self.config.motors.len();
        let channel = match request.params.get("channel").and_then(|v| v.as_u64()) {
            Some(ch) if (ch as usize) < n_motors => ch as usize,
            Some(ch) => {
                return Response::err(
                    request.id.clone(),
                    "INVALID_PARAMS",
                    format!("channel {ch} is not configured; {n_motors} motor(s) available"),
                );
            }
            None => {
                return Response::err(request.id.clone(), "INVALID_PARAMS", "channel is required");
            }
        };

        // Validate optional fields before locking the store.
        if let Some(v) = request.params.get("speed_scale").and_then(|v| v.as_f64())
            && !CalibrationStore::valid_speed_scale(v)
        {
            return Response::err(
                request.id.clone(),
                "INVALID_PARAMS",
                "speed_scale must be in [0.5, 2.0]",
            );
        }
        if let Some(v) = request.params.get("deadband_pct").and_then(|v| v.as_f64())
            && !CalibrationStore::valid_deadband_pct(v)
        {
            return Response::err(
                request.id.clone(),
                "INVALID_PARAMS",
                "deadband_pct must be in [0.0, 20.0]",
            );
        }

        let mut guard = self.calibration.lock().await;
        let entry = &mut guard.motors[channel];

        if let Some(v) = request.params.get("speed_scale").and_then(|v| v.as_f64()) {
            entry.speed_scale = v;
        }
        if let Some(v) = request.params.get("deadband_pct").and_then(|v| v.as_f64()) {
            entry.deadband_pct = v;
        }
        if let Some(v) = request.params.get("reversed").and_then(|v| v.as_bool()) {
            entry.reversed = v;
        }
        let updated = entry.clone();
        drop(guard);

        Response::ok(
            request.id.clone(),
            json!({
                "channel": channel,
                "speed_scale": updated.speed_scale,
                "deadband_pct": updated.deadband_pct,
                "reversed": updated.reversed,
            }),
        )
    }

    /// `set_servo_calibration { servo, trim_us }` — set trim offset for a
    /// named servo.  Valid servo names: `"steering"`, `"camera_pan"`,
    /// `"camera_tilt"`.
    async fn handle_set_servo_calibration(&self, request: &Request) -> Response {
        let servo_name = match request.params.get("servo").and_then(|v| v.as_str()) {
            Some(s) if matches!(s, "steering" | "camera_pan" | "camera_tilt") => s.to_owned(),
            Some(s) => {
                return Response::err(
                    request.id.clone(),
                    "INVALID_PARAMS",
                    format!("servo '{s}' not recognised; valid: steering, camera_pan, camera_tilt"),
                );
            }
            None => {
                return Response::err(request.id.clone(), "INVALID_PARAMS", "servo is required");
            }
        };
        let trim_us = match request.params.get("trim_us").and_then(|v| v.as_i64()) {
            Some(v) if (-500..=500).contains(&v) => v as i16,
            Some(_) => {
                return Response::err(
                    request.id.clone(),
                    "INVALID_PARAMS",
                    "trim_us must be in [-500, 500]",
                );
            }
            None => {
                return Response::err(
                    request.id.clone(),
                    "INVALID_PARAMS",
                    "trim_us is required and must be an integer",
                );
            }
        };

        let mut guard = self.calibration.lock().await;
        guard.servos.entry(servo_name.clone()).or_default().trim_us = trim_us;
        drop(guard);

        Response::ok(
            request.id.clone(),
            json!({ "servo": servo_name, "trim_us": trim_us }),
        )
    }

    /// `calibrate_grayscale { channel, surface }` — read a live ADC value for
    /// one sensor position and store it as the white or black surface reference.
    ///
    /// `channel` is the **sensor position index** (0–2), not the ADC bus channel.
    async fn handle_calibrate_grayscale(&self, request: &Request) -> Response {
        let sensor_idx = match request.params.get("channel").and_then(|v| v.as_u64()) {
            Some(ch) if ch <= 2 => ch as usize,
            Some(_) | None => {
                return Response::err(
                    request.id.clone(),
                    "INVALID_PARAMS",
                    "channel is required and must be 0–2",
                );
            }
        };
        let surface = match request.params.get("surface").and_then(|v| v.as_str()) {
            Some(s) if matches!(s, "white" | "black") => s.to_owned(),
            Some(s) => {
                return Response::err(
                    request.id.clone(),
                    "INVALID_PARAMS",
                    format!("surface '{s}' not recognised; valid: white, black"),
                );
            }
            None => {
                return Response::err(request.id.clone(), "INVALID_PARAMS", "surface is required");
            }
        };

        let adc_channel = self.config.sensors.grayscale[sensor_idx];

        // Read live ADC value before locking the store.
        let raw_value = match adc::read_adc(&self.hat, adc_channel).await {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, adc_channel, "calibrate_grayscale ADC read failed");
                return Response::err(request.id.clone(), e.error_code(), e.to_string());
            }
        };

        // Validate the constraint before storing.
        let mut guard = self.calibration.lock().await;
        if surface == "white" {
            let black_raw = guard.grayscale[sensor_idx].black_raw;
            if !CalibrationStore::valid_grayscale(raw_value, black_raw) {
                return Response::err(
                    request.id.clone(),
                    "INVALID_PARAMS",
                    format!(
                        "white_raw ({raw_value}) must be < current black_raw ({black_raw}); \
                         recapture black surface first or adjust surfaces",
                    ),
                );
            }
            guard.grayscale[sensor_idx].white_raw = raw_value;
        } else {
            let white_raw = guard.grayscale[sensor_idx].white_raw;
            if !CalibrationStore::valid_grayscale(white_raw, raw_value) {
                return Response::err(
                    request.id.clone(),
                    "INVALID_PARAMS",
                    format!(
                        "black_raw ({raw_value}) must be > current white_raw ({white_raw}); \
                         recapture white surface first or adjust surfaces",
                    ),
                );
            }
            guard.grayscale[sensor_idx].black_raw = raw_value;
        }

        Response::ok(
            request.id.clone(),
            json!({
                "channel": sensor_idx,
                "adc_channel": adc_channel,
                "surface": surface,
                "raw_value": raw_value,
                "stored": true,
            }),
        )
    }

    /// `read_grayscale_normalized {}` — read all three grayscale sensor
    /// channels and return per-channel values normalised against the captured
    /// surface calibration.
    async fn handle_read_grayscale_normalized(&self, request: &Request) -> Response {
        let [ch0, ch1, ch2] = self.config.sensors.grayscale;

        // Copy calibration before any hardware calls.
        let cal_gs = {
            let guard = self.calibration.lock().await;
            guard.grayscale.clone()
        };

        let mut raw_values = [0u16; 3];
        for (i, ch) in [ch0, ch1, ch2].iter().enumerate() {
            match adc::read_adc(&self.hat, *ch).await {
                Ok(v) => raw_values[i] = v,
                Err(e) => {
                    warn!(error = %e, channel = ch, "read_grayscale_normalized ADC read failed");
                    return Response::err(
                        request.id.clone(),
                        e.error_code(),
                        format!("grayscale channel {ch}: {e}"),
                    );
                }
            }
        }

        let normalized: Vec<f64> = raw_values
            .iter()
            .zip(cal_gs.iter())
            .map(|(&raw, gs)| {
                let white = gs.white_raw as f64;
                let black = gs.black_raw as f64;
                ((raw as f64 - white) / (black - white)).clamp(0.0, 1.0)
            })
            .collect();

        Response::ok(
            request.id.clone(),
            json!({
                "channels": [ch0, ch1, ch2],
                "normalized": normalized,
            }),
        )
    }

    /// `save_calibration {}` — persist the current in-memory calibration store
    /// to disk at `config.calibration_path`.
    async fn handle_save_calibration(&self, request: &Request) -> Response {
        let store_snapshot = {
            let guard = self.calibration.lock().await;
            guard.clone()
        };
        let path = self.config.calibration_path.clone();
        let path_display = path.display().to_string();
        match tokio::task::spawn_blocking(move || store_snapshot.save(&path)).await {
            Ok(Ok(())) => {
                tracing::info!(path = %path_display, "calibration saved");
                Response::ok(
                    request.id.clone(),
                    json!({
                        "saved": true,
                        "path": path_display,
                    }),
                )
            }
            Ok(Err(e)) => {
                warn!(error = %e, path = %path_display, "save_calibration failed");
                Response::err(request.id.clone(), "HARDWARE_ERROR", e.to_string())
            }
            Err(e) => Response::err(request.id.clone(), "INTERNAL_ERROR", e.to_string()),
        }
    }

    /// `reset_calibration {}` — replace in-memory store with factory defaults.
    /// The file on disk is NOT overwritten; call `save_calibration` to persist.
    async fn handle_reset_calibration(&self, request: &Request) -> Response {
        let n_motors = self.config.motors.len();
        let fresh = CalibrationStore::default_for(n_motors);
        let mut guard = self.calibration.lock().await;
        *guard = fresh;
        drop(guard);
        Response::ok(request.id.clone(), json!({ "reset": true }))
    }

    // -----------------------------------------------------------------------
    // Routine methods
    // -----------------------------------------------------------------------

    /// `start_routine { name, speed_pct?, obstacle_threshold_cm?,
    ///   cliff_threshold_normalized?, max_duration_s? }` — start a named
    /// autonomous routine with optional per-run overrides.
    async fn handle_start_routine(&self, request: &Request) -> Response {
        let name = match request.params.get("name").and_then(|v| v.as_str()) {
            Some(n) => n.to_owned(),
            None => {
                return Response::err(request.id.clone(), "INVALID_PARAMS", "name is required");
            }
        };

        // Optional float overrides — validate if present.
        let speed_pct = if let Some(s) = request.params.get("speed_pct").and_then(|v| v.as_f64()) {
            if !(1.0..=100.0).contains(&s) {
                return Response::err(
                    request.id.clone(),
                    "INVALID_PARAMS",
                    format!("speed_pct {s} is out of range 1.0–100.0"),
                );
            }
            Some(s)
        } else {
            None
        };

        let obstacle_threshold_cm = if let Some(v) = request
            .params
            .get("obstacle_threshold_cm")
            .and_then(|v| v.as_f64())
        {
            if v <= 0.0 {
                return Response::err(
                    request.id.clone(),
                    "INVALID_PARAMS",
                    "obstacle_threshold_cm must be > 0.0",
                );
            }
            Some(v)
        } else {
            None
        };

        let cliff_threshold_normalized = if let Some(v) = request
            .params
            .get("cliff_threshold_normalized")
            .and_then(|v| v.as_f64())
        {
            if !(0.0..=1.0).contains(&v) {
                return Response::err(
                    request.id.clone(),
                    "INVALID_PARAMS",
                    format!("cliff_threshold_normalized {v} is out of range 0.0–1.0"),
                );
            }
            Some(v)
        } else {
            None
        };

        let max_duration_s = request
            .params
            .get("max_duration_s")
            .and_then(|v| v.as_u64());

        let mut engine = self.routine.lock().await;
        match engine.start(
            &name,
            speed_pct,
            obstacle_threshold_cm,
            cliff_threshold_normalized,
            max_duration_s,
        ) {
            Ok(()) => Response::ok(
                request.id.clone(),
                json!({ "name": name, "started_at_uptime_s": self.start_time.elapsed().as_secs() }),
            ),
            Err("ALREADY_RUNNING") => Response::err(
                request.id.clone(),
                "ALREADY_RUNNING",
                "a routine is already running",
            ),
            Err(_) => Response::err(
                request.id.clone(),
                "INVALID_PARAMS",
                format!("routine '{}' is not recognised; valid: explore", name),
            ),
        }
    }

    /// `stop_routine {}` — stop the active routine and return final stats.
    async fn handle_stop_routine(&self, request: &Request) -> Response {
        // Phase 1: Extract the active routine under the lock, then drop it
        // so concurrent start/status calls are not blocked during the async wait.
        let active = {
            let mut engine = self.routine.lock().await;
            engine.take_active()
        };

        let Some((name, started_at, stop_flag, mut handle)) = active else {
            return Response::err(
                request.id.clone(),
                "INVALID_PARAMS",
                "no routine is currently running",
            );
        };

        // Phase 2: Signal stop and await the task outside the lock.
        use std::sync::atomic::Ordering;
        stop_flag.store(true, Ordering::Relaxed);
        let ran_for_s = started_at.elapsed().as_secs();

        let (stats, stop_reason) = match tokio::time::timeout(Duration::from_secs(2), &mut handle)
            .await
        {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => (crate::routine::RoutineStats::default(), "error".to_string()),
            Err(_timeout) => {
                // Timed out — abort the detached task and idle motors.
                handle.abort();
                let _ = handle.await;
                for cfg in &self.config.motors {
                    if let Err(e) = motor::idle_motor(&self.hat, cfg.pwm_channel).await {
                        error!(error = %e, pwm_channel = cfg.pwm_channel, "stop_routine: SAFETY: failed to idle motor after task abort");
                    }
                }
                self.motor_lease_manager
                    .release_connection(crate::routine::ROUTINE_CONN_ID)
                    .await;
                (
                    crate::routine::RoutineStats::default(),
                    "timeout_abort".to_string(),
                )
            }
        };

        Response::ok(
            request.id.clone(),
            json!({
                "name": name,
                "ran_for_s": ran_for_s,
                "obstacles_avoided": stats.obstacles_avoided,
                "cliffs_avoided": stats.cliffs_avoided,
                "stop_reason": stop_reason,
            }),
        )
    }

    /// `get_routine_status {}` — return current routine state without
    /// affecting it.
    async fn handle_get_routine_status(&self, request: &Request) -> Response {
        let engine = self.routine.lock().await;
        let s = engine.status();
        drop(engine);
        Response::ok(
            request.id.clone(),
            json!({
                "running": s.running,
                "name": s.name,
                "elapsed_s": s.elapsed_s,
                "obstacles_avoided": s.obstacles_avoided,
                "cliffs_avoided": s.cliffs_avoided,
            }),
        )
    }
}

/// Extract and validate `channel` (0–11) from request params.
fn extract_channel(request: &Request) -> Result<u8, Response> {
    match request.params.get("channel").and_then(|v| v.as_u64()) {
        Some(ch) if ch <= 11 => Ok(ch as u8),
        Some(_) | None => Err(Response::err(
            request.id.clone(),
            "INVALID_PARAMS",
            "channel is required and must be 0–11",
        )),
    }
}

/// Extract and validate `ttl_ms` (100–5000) from request params, using
/// `default_ttl_ms` when the key is absent. The default is clamped to the
/// same 100–5000 ms range so misconfigured defaults are handled gracefully.
fn extract_ttl(request: &Request, default_ttl_ms: u64) -> Result<u64, Response> {
    const TTL_MIN_MS: u64 = 100;
    const TTL_MAX_MS: u64 = 5000;
    match request.params.get("ttl_ms") {
        None | Some(serde_json::Value::Null) => Ok(default_ttl_ms.clamp(TTL_MIN_MS, TTL_MAX_MS)),
        Some(v) => match v.as_u64() {
            Some(ms) if (TTL_MIN_MS..=TTL_MAX_MS).contains(&ms) => Ok(ms),
            _ => Err(Response::err(
                request.id.clone(),
                "INVALID_PARAMS",
                "ttl_ms must be an integer 100–5000",
            )),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hat::gpio::{GpioBus, GpioError, HatGpio};
    use crate::hat::i2c::Hat;
    use crate::testing::{MockAlsaControl, MockGpio, MockI2c};

    fn test_handler() -> Handler {
        let hat = Arc::new(Hat::new(MockI2c { response: [0, 0] }, 0x14));
        let gpio = Arc::new(HatGpio::new(MockGpio::new()));
        Handler::new(Arc::new(Config::default()), hat, gpio)
    }

    fn test_handler_with_adc(hi: u8, lo: u8) -> Handler {
        let hat = Arc::new(Hat::new(MockI2c { response: [hi, lo] }, 0x14));
        let gpio = Arc::new(HatGpio::new(MockGpio::new()));
        Handler::new(Arc::new(Config::default()), hat, gpio)
    }

    fn test_handler_with_config(config: Arc<Config>) -> Handler {
        let hat = Arc::new(Hat::new(MockI2c { response: [0, 0] }, 0x14));
        let gpio = Arc::new(HatGpio::new(MockGpio::new()));
        Handler::new(config, hat, gpio)
    }

    // ------------------------------------------------------------------
    // Mock GPIO that simulates an HC-SR04 ECHO pulse for handler-level
    // ultrasonic tests.  All other pins behave like MockGpio.
    // ------------------------------------------------------------------

    struct MockUltrasonicGpio {
        state: std::collections::HashMap<u8, bool>,
        echo_bcm: u8,
        reads_until_high: usize,
        reads_high_for: usize,
        read_count: usize,
    }

    impl MockUltrasonicGpio {
        fn new(echo_bcm: u8, reads_until_high: usize, reads_high_for: usize) -> Self {
            Self {
                state: std::collections::HashMap::new(),
                echo_bcm,
                reads_until_high,
                reads_high_for,
                read_count: 0,
            }
        }
    }

    impl GpioBus for MockUltrasonicGpio {
        fn write_pin(&mut self, pin_bcm: u8, high: bool) -> Result<(), GpioError> {
            self.state.insert(pin_bcm, high);
            Ok(())
        }

        fn read_pin(&mut self, pin_bcm: u8) -> Result<bool, GpioError> {
            if pin_bcm == self.echo_bcm {
                let count = self.read_count;
                self.read_count += 1;
                return Ok(count >= self.reads_until_high
                    && count < self.reads_until_high + self.reads_high_for);
            }
            Ok(*self.state.get(&pin_bcm).unwrap_or(&false))
        }
    }

    fn test_handler_with_ultrasonic(mock: MockUltrasonicGpio) -> Handler {
        let hat = Arc::new(Hat::new(MockI2c { response: [0, 0] }, 0x14));
        let gpio = Arc::new(HatGpio::new(mock));
        Handler::new(Arc::new(Config::default()), hat, gpio)
    }

    #[tokio::test]
    async fn health_returns_ok() {
        let handler = test_handler();
        let raw = r#"{"id":"1","method":"health","params":{}}"#;
        let resp_str = handler.dispatch(raw, 0).await;
        let resp: serde_json::Value = serde_json::from_str(&resp_str).unwrap();

        assert_eq!(resp["id"], "1");
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["result"]["status"], "ok");
        assert_eq!(resp["result"]["schema_version"], "1.0.0");
        assert_eq!(resp["result"]["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(resp["result"]["hat_address"], "0x14");
        assert_eq!(resp["result"]["i2c_bus"], 1);
        assert!(resp["result"]["uptime_s"].is_number());
    }

    #[tokio::test]
    async fn unknown_method_returns_error() {
        let handler = test_handler();
        let raw = r#"{"id":"2","method":"explode","params":{}}"#;
        let resp_str = handler.dispatch(raw, 0).await;
        let resp: serde_json::Value = serde_json::from_str(&resp_str).unwrap();

        assert_eq!(resp["id"], "2");
        assert_eq!(resp["ok"], false);
        assert_eq!(resp["error"]["code"], "UNKNOWN_METHOD");
    }

    #[tokio::test]
    async fn malformed_json_returns_error() {
        let handler = test_handler();
        let raw = "this is not json";
        let resp_str = handler.dispatch(raw, 0).await;
        let resp: serde_json::Value = serde_json::from_str(&resp_str).unwrap();

        assert_eq!(resp["ok"], false);
        assert_eq!(resp["error"]["code"], "INVALID_PARAMS");
    }

    #[tokio::test]
    async fn missing_method_field_returns_error() {
        let handler = test_handler();
        let raw = r#"{"id":"3"}"#;
        let resp_str = handler.dispatch(raw, 0).await;
        let resp: serde_json::Value = serde_json::from_str(&resp_str).unwrap();

        assert_eq!(resp["ok"], false);
        assert_eq!(resp["error"]["code"], "INVALID_PARAMS");
    }

    #[tokio::test]
    async fn params_default_to_null() {
        let handler = test_handler();
        // Valid request without params field — should still dispatch.
        let raw = r#"{"id":"4","method":"health"}"#;
        let resp_str = handler.dispatch(raw, 0).await;
        let resp: serde_json::Value = serde_json::from_str(&resp_str).unwrap();

        assert_eq!(resp["ok"], true);
        assert_eq!(resp["result"]["status"], "ok");
    }

    #[tokio::test]
    async fn get_battery_voltage_returns_scaled_voltage() {
        // raw = 0x0FFF = 4095 (12-bit max) → voltage_v = (4095/4095) × 3.3 × 3.0 = 9.9 V
        let handler = test_handler_with_adc(0x0F, 0xFF);
        let raw = r#"{"id":"5","method":"get_battery_voltage","params":{}}"#;
        let resp_str = handler.dispatch(raw, 0).await;
        let resp: serde_json::Value = serde_json::from_str(&resp_str).unwrap();

        assert_eq!(resp["id"], "5");
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["result"]["voltage_v"], 9.9_f64);
    }

    #[tokio::test]
    async fn set_servo_pulse_us_returns_channel_and_pulse() {
        let handler = test_handler();
        let raw =
            r#"{"id":"6","method":"set_servo_pulse_us","params":{"channel":0,"pulse_us":1500}}"#;
        let resp_str = handler.dispatch(raw, 1).await;
        let resp: serde_json::Value = serde_json::from_str(&resp_str).unwrap();

        assert_eq!(resp["id"], "6");
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["result"]["channel"], 0);
        assert_eq!(resp["result"]["pulse_us"], 1500);
    }

    #[tokio::test]
    async fn set_servo_angle_returns_channel_angle_and_pulse() {
        let handler = test_handler();
        let raw =
            r#"{"id":"7","method":"set_servo_angle","params":{"channel":2,"angle_deg":90.0}}"#;
        let resp_str = handler.dispatch(raw, 1).await;
        let resp: serde_json::Value = serde_json::from_str(&resp_str).unwrap();

        assert_eq!(resp["id"], "7");
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["result"]["channel"], 2);
        assert_eq!(resp["result"]["angle_deg"], 90.0_f64);
        assert_eq!(resp["result"]["pulse_us"], 1500);
    }

    #[tokio::test]
    async fn set_servo_pulse_us_invalid_channel_returns_invalid_params() {
        let handler = test_handler();
        let raw =
            r#"{"id":"8","method":"set_servo_pulse_us","params":{"channel":12,"pulse_us":1500}}"#;
        let resp_str = handler.dispatch(raw, 1).await;
        let resp: serde_json::Value = serde_json::from_str(&resp_str).unwrap();

        assert_eq!(resp["ok"], false);
        assert_eq!(resp["error"]["code"], "INVALID_PARAMS");
    }

    #[tokio::test]
    async fn set_servo_angle_invalid_angle_returns_invalid_params() {
        let handler = test_handler();
        let raw =
            r#"{"id":"9","method":"set_servo_angle","params":{"channel":0,"angle_deg":200.0}}"#;
        let resp_str = handler.dispatch(raw, 1).await;
        let resp: serde_json::Value = serde_json::from_str(&resp_str).unwrap();

        assert_eq!(resp["ok"], false);
        assert_eq!(resp["error"]["code"], "INVALID_PARAMS");
    }

    #[tokio::test]
    async fn reset_mcu_returns_reset_ms() {
        let handler = test_handler();
        let raw = r#"{"id":"r1","method":"reset_mcu","params":{}}"#;
        let resp_str = handler.dispatch(raw, 0).await;
        let resp: serde_json::Value = serde_json::from_str(&resp_str).unwrap();

        assert_eq!(resp["id"], "r1");
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["result"]["reset_ms"], crate::reset::RESET_HOLD_MS);
    }

    #[tokio::test]
    async fn reset_mcu_rate_limited_on_rapid_retry() {
        let handler = test_handler();
        let raw = r#"{"id":"r2","method":"reset_mcu","params":{}}"#;
        // First call succeeds.
        handler.dispatch(raw, 0).await;
        // Immediate second call must be rate-limited.
        let resp_str = handler.dispatch(raw, 0).await;
        let resp: serde_json::Value = serde_json::from_str(&resp_str).unwrap();

        assert_eq!(resp["ok"], false);
        assert_eq!(resp["error"]["code"], "HARDWARE_ERROR");
        assert!(
            resp["error"]["message"]
                .as_str()
                .unwrap()
                .contains("rate-limited")
        );
    }

    #[tokio::test]
    async fn read_gpio_sw_returns_low_by_default() {
        let handler = test_handler();
        let raw = r#"{"id":"g1","method":"read_gpio","params":{"pin":"SW"}}"#;
        let resp_str = handler.dispatch(raw, 0).await;
        let resp: serde_json::Value = serde_json::from_str(&resp_str).unwrap();

        assert_eq!(resp["id"], "g1");
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["result"]["pin"], "SW");
        assert_eq!(resp["result"]["high"], false);
    }

    #[tokio::test]
    async fn write_gpio_d4_returns_ok() {
        let handler = test_handler();
        let raw = r#"{"id":"g2","method":"write_gpio","params":{"pin":"D4","high":true}}"#;
        let resp_str = handler.dispatch(raw, 0).await;
        let resp: serde_json::Value = serde_json::from_str(&resp_str).unwrap();

        assert_eq!(resp["id"], "g2");
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["result"]["pin"], "D4");
        assert_eq!(resp["result"]["high"], true);
    }

    #[tokio::test]
    async fn write_gpio_input_pin_returns_invalid_params() {
        let handler = test_handler();
        let raw = r#"{"id":"g3","method":"write_gpio","params":{"pin":"SW","high":true}}"#;
        let resp_str = handler.dispatch(raw, 0).await;
        let resp: serde_json::Value = serde_json::from_str(&resp_str).unwrap();

        assert_eq!(resp["ok"], false);
        assert_eq!(resp["error"]["code"], "INVALID_PARAMS");
    }

    #[tokio::test]
    async fn read_gpio_unknown_pin_returns_invalid_params() {
        let handler = test_handler();
        let raw = r#"{"id":"g4","method":"read_gpio","params":{"pin":"BADPIN"}}"#;
        let resp_str = handler.dispatch(raw, 0).await;
        let resp: serde_json::Value = serde_json::from_str(&resp_str).unwrap();

        assert_eq!(resp["ok"], false);
        assert_eq!(resp["error"]["code"], "INVALID_PARAMS");
    }

    #[tokio::test]
    async fn read_adc_valid_channel_returns_raw_value() {
        // raw bytes 0x00, 0x2A → u16 = 42
        let handler = test_handler_with_adc(0x00, 0x2A);
        let raw = r#"{"id":"a1","method":"read_adc","params":{"channel":0}}"#;
        let resp_str = handler.dispatch(raw, 0).await;
        let resp: serde_json::Value = serde_json::from_str(&resp_str).unwrap();

        assert_eq!(resp["id"], "a1");
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["result"]["channel"], 0);
        assert_eq!(resp["result"]["raw_value"], 42);
    }

    #[tokio::test]
    async fn read_adc_max_channel_is_accepted() {
        let handler = test_handler_with_adc(0x01, 0x00);
        let raw = r#"{"id":"a2","method":"read_adc","params":{"channel":7}}"#;
        let resp_str = handler.dispatch(raw, 0).await;
        let resp: serde_json::Value = serde_json::from_str(&resp_str).unwrap();

        assert_eq!(resp["ok"], true);
        assert_eq!(resp["result"]["channel"], 7);
    }

    #[tokio::test]
    async fn read_adc_invalid_channel_returns_invalid_params() {
        let handler = test_handler();
        let raw = r#"{"id":"a3","method":"read_adc","params":{"channel":8}}"#;
        let resp_str = handler.dispatch(raw, 0).await;
        let resp: serde_json::Value = serde_json::from_str(&resp_str).unwrap();

        assert_eq!(resp["ok"], false);
        assert_eq!(resp["error"]["code"], "INVALID_PARAMS");
    }

    #[tokio::test]
    async fn get_servo_status_empty_returns_no_leases() {
        let handler = test_handler();
        let raw = r#"{"id":"v1","method":"get_servo_status","params":{}}"#;
        let resp_str = handler.dispatch(raw, 0).await;
        let resp: serde_json::Value = serde_json::from_str(&resp_str).unwrap();

        assert_eq!(resp["id"], "v1");
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["result"]["active_leases"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn get_servo_status_after_set_returns_lease() {
        let handler = test_handler();
        // Register a lease on channel 3 owned by conn_id 42.
        let set_raw =
            r#"{"id":"6","method":"set_servo_pulse_us","params":{"channel":3,"pulse_us":1500}}"#;
        handler.dispatch(set_raw, 42).await;
        let raw = r#"{"id":"v2","method":"get_servo_status","params":{}}"#;
        let resp_str = handler.dispatch(raw, 0).await;
        let resp: serde_json::Value = serde_json::from_str(&resp_str).unwrap();

        assert_eq!(resp["ok"], true);
        let leases = resp["result"]["active_leases"].as_array().unwrap();
        assert_eq!(leases.len(), 1);
        assert_eq!(leases[0]["channel"], 3);
        assert_eq!(leases[0]["conn_id"], 42);
        assert!(leases[0]["ttl_remaining_ms"].is_number());
    }

    #[tokio::test]
    async fn get_mcu_status_no_resets() {
        let handler = test_handler();
        let raw = r#"{"id":"m1","method":"get_mcu_status","params":{}}"#;
        let resp_str = handler.dispatch(raw, 0).await;
        let resp: serde_json::Value = serde_json::from_str(&resp_str).unwrap();

        assert_eq!(resp["id"], "m1");
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["result"]["resets_since_start"], 0);
        assert!(resp["result"]["last_reset_s_ago"].is_null());
    }

    #[tokio::test]
    async fn get_mcu_status_after_reset() {
        let handler = test_handler();
        handler
            .dispatch(r#"{"id":"r1","method":"reset_mcu","params":{}}"#, 0)
            .await;
        let raw = r#"{"id":"m2","method":"get_mcu_status","params":{}}"#;
        let resp_str = handler.dispatch(raw, 0).await;
        let resp: serde_json::Value = serde_json::from_str(&resp_str).unwrap();

        assert_eq!(resp["ok"], true);
        assert_eq!(resp["result"]["resets_since_start"], 1);
        assert!(resp["result"]["last_reset_s_ago"].is_number());
    }

    // ------------------------------------------------------------------
    // Motor IPC handler tests
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn set_motor_speed_valid_returns_channel_and_speed() {
        let handler = test_handler();
        // Default config has motor channel 0 = pwm_channel 12, dir_pin BCM 24.
        let raw =
            r#"{"id":"mo1","method":"set_motor_speed","params":{"channel":0,"speed_pct":50.0}}"#;
        let resp_str = handler.dispatch(raw, 1).await;
        let resp: serde_json::Value = serde_json::from_str(&resp_str).unwrap();

        assert_eq!(resp["id"], "mo1");
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["result"]["channel"], 0);
        assert_eq!(resp["result"]["speed_pct"], 50.0_f64);
    }

    #[tokio::test]
    async fn set_motor_speed_channel_out_of_range_returns_invalid_params() {
        let handler = test_handler();
        // Default config has 2 motors (indices 0 and 1); index 2 is not configured.
        let raw =
            r#"{"id":"mo2","method":"set_motor_speed","params":{"channel":2,"speed_pct":50.0}}"#;
        let resp_str = handler.dispatch(raw, 1).await;
        let resp: serde_json::Value = serde_json::from_str(&resp_str).unwrap();

        assert_eq!(resp["ok"], false);
        assert_eq!(resp["error"]["code"], "INVALID_PARAMS");
    }

    #[tokio::test]
    async fn set_motor_speed_out_of_range_speed_returns_invalid_params() {
        let handler = test_handler();
        let raw =
            r#"{"id":"mo3","method":"set_motor_speed","params":{"channel":0,"speed_pct":150.0}}"#;
        let resp_str = handler.dispatch(raw, 1).await;
        let resp: serde_json::Value = serde_json::from_str(&resp_str).unwrap();

        assert_eq!(resp["ok"], false);
        assert_eq!(resp["error"]["code"], "INVALID_PARAMS");
    }

    #[tokio::test]
    async fn stop_all_motors_returns_stopped_count() {
        let handler = test_handler();
        let raw = r#"{"id":"mo4","method":"stop_all_motors","params":{}}"#;
        let resp_str = handler.dispatch(raw, 0).await;
        let resp: serde_json::Value = serde_json::from_str(&resp_str).unwrap();

        assert_eq!(resp["id"], "mo4");
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["result"]["stopped"], 2); // default config has 2 motors
    }

    #[tokio::test]
    async fn get_motor_status_empty_returns_no_leases() {
        let handler = test_handler();
        let raw = r#"{"id":"mo5","method":"get_motor_status","params":{}}"#;
        let resp_str = handler.dispatch(raw, 0).await;
        let resp: serde_json::Value = serde_json::from_str(&resp_str).unwrap();

        assert_eq!(resp["id"], "mo5");
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["result"]["active_leases"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn get_motor_status_after_set_returns_lease() {
        let handler = test_handler();
        let set_raw =
            r#"{"id":"mo1","method":"set_motor_speed","params":{"channel":1,"speed_pct":75.0}}"#;
        handler.dispatch(set_raw, 99).await;
        let raw = r#"{"id":"mo6","method":"get_motor_status","params":{}}"#;
        let resp_str = handler.dispatch(raw, 0).await;
        let resp: serde_json::Value = serde_json::from_str(&resp_str).unwrap();

        assert_eq!(resp["ok"], true);
        let leases = resp["result"]["active_leases"].as_array().unwrap();
        assert_eq!(leases.len(), 1);
        assert_eq!(leases[0]["channel"], 1);
        assert_eq!(leases[0]["conn_id"], 99);
    }

    // ------------------------------------------------------------------
    // Convenience / coordinated method tests
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn drive_sets_both_motors_and_returns_speed_and_count() {
        let handler = test_handler();
        let raw = r#"{"id":"d1","method":"drive","params":{"speed_pct":60.0}}"#;
        let resp_str = handler.dispatch(raw, 1).await;
        let resp: serde_json::Value = serde_json::from_str(&resp_str).unwrap();

        assert_eq!(resp["id"], "d1");
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["result"]["speed_pct"], 60.0_f64);
        assert_eq!(resp["result"]["motors"], 2); // default config has 2 motors
    }

    #[tokio::test]
    async fn drive_creates_leases_for_all_motors() {
        let handler = test_handler();
        handler
            .dispatch(
                r#"{"id":"d1","method":"drive","params":{"speed_pct":30.0}}"#,
                7,
            )
            .await;
        let status_str = handler
            .dispatch(r#"{"id":"d2","method":"get_motor_status","params":{}}"#, 0)
            .await;
        let status: serde_json::Value = serde_json::from_str(&status_str).unwrap();
        let leases = status["result"]["active_leases"].as_array().unwrap();
        assert_eq!(leases.len(), 2);
    }

    #[tokio::test]
    async fn drive_invalid_speed_returns_invalid_params() {
        let handler = test_handler();
        let raw = r#"{"id":"d3","method":"drive","params":{"speed_pct":200.0}}"#;
        let resp_str = handler.dispatch(raw, 1).await;
        let resp: serde_json::Value = serde_json::from_str(&resp_str).unwrap();

        assert_eq!(resp["ok"], false);
        assert_eq!(resp["error"]["code"], "INVALID_PARAMS");
    }

    #[tokio::test]
    async fn steer_sets_steering_servo_by_name() {
        let handler = test_handler();
        // Default config: steering = channel 2
        let raw = r#"{"id":"s1","method":"steer","params":{"angle_deg":90.0}}"#;
        let resp_str = handler.dispatch(raw, 1).await;
        let resp: serde_json::Value = serde_json::from_str(&resp_str).unwrap();

        assert_eq!(resp["id"], "s1");
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["result"]["servo"], "steering");
        assert_eq!(resp["result"]["channel"], 2);
        assert_eq!(resp["result"]["angle_deg"], 90.0_f64);
        assert_eq!(resp["result"]["pulse_us"], 1500);
    }

    #[tokio::test]
    async fn steer_invalid_angle_returns_invalid_params() {
        let handler = test_handler();
        let raw = r#"{"id":"s2","method":"steer","params":{"angle_deg":200.0}}"#;
        let resp_str = handler.dispatch(raw, 1).await;
        let resp: serde_json::Value = serde_json::from_str(&resp_str).unwrap();

        assert_eq!(resp["ok"], false);
        assert_eq!(resp["error"]["code"], "INVALID_PARAMS");
    }

    #[tokio::test]
    async fn pan_camera_sets_pan_servo_by_name() {
        let handler = test_handler();
        // Default config: camera_pan = channel 0
        let raw = r#"{"id":"p1","method":"pan_camera","params":{"angle_deg":45.0}}"#;
        let resp_str = handler.dispatch(raw, 1).await;
        let resp: serde_json::Value = serde_json::from_str(&resp_str).unwrap();

        assert_eq!(resp["ok"], true);
        assert_eq!(resp["result"]["servo"], "camera_pan");
        assert_eq!(resp["result"]["channel"], 0);
        assert_eq!(resp["result"]["angle_deg"], 45.0_f64);
    }

    #[tokio::test]
    async fn tilt_camera_sets_tilt_servo_by_name() {
        let handler = test_handler();
        // Default config: camera_tilt = channel 1
        let raw = r#"{"id":"t1","method":"tilt_camera","params":{"angle_deg":120.0}}"#;
        let resp_str = handler.dispatch(raw, 1).await;
        let resp: serde_json::Value = serde_json::from_str(&resp_str).unwrap();

        assert_eq!(resp["ok"], true);
        assert_eq!(resp["result"]["servo"], "camera_tilt");
        assert_eq!(resp["result"]["channel"], 1);
        assert_eq!(resp["result"]["angle_deg"], 120.0_f64);
    }

    #[tokio::test]
    async fn read_grayscale_returns_three_channel_values() {
        // raw bytes 0x00, 0x2A → u16 = 42 (returned for every ADC read)
        let handler = test_handler_with_adc(0x00, 0x2A);
        let raw = r#"{"id":"gs1","method":"read_grayscale","params":{}}"#;
        let resp_str = handler.dispatch(raw, 0).await;
        let resp: serde_json::Value = serde_json::from_str(&resp_str).unwrap();

        assert_eq!(resp["id"], "gs1");
        assert_eq!(resp["ok"], true);
        // Default grayscale channels are [0, 1, 2]
        assert_eq!(resp["result"]["channels"], serde_json::json!([0, 1, 2]));
        assert_eq!(resp["result"]["values"], serde_json::json!([42, 42, 42]));
    }

    #[tokio::test]
    async fn drive_missing_speed_returns_invalid_params() {
        let handler = test_handler();
        let raw = r#"{"id":"d4","method":"drive","params":{}}"#;
        let resp_str = handler.dispatch(raw, 1).await;
        let resp: serde_json::Value = serde_json::from_str(&resp_str).unwrap();

        assert_eq!(resp["ok"], false);
        assert_eq!(resp["error"]["code"], "INVALID_PARAMS");
    }

    #[tokio::test]
    async fn steer_missing_angle_returns_invalid_params() {
        let handler = test_handler();
        let raw = r#"{"id":"s3","method":"steer","params":{}}"#;
        let resp_str = handler.dispatch(raw, 1).await;
        let resp: serde_json::Value = serde_json::from_str(&resp_str).unwrap();

        assert_eq!(resp["ok"], false);
        assert_eq!(resp["error"]["code"], "INVALID_PARAMS");
    }

    // ------------------------------------------------------------------
    // read_ultrasonic
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn read_ultrasonic_timeout_returns_timeout_error() {
        // MockGpio's ECHO pin is always low (never written) → driver times out.
        // Use a very short timeout so the test completes quickly.
        let mut config = Config::default();
        config.ultrasonic.timeout_ms = 1;
        let handler = test_handler_with_config(Arc::new(config));

        let raw = r#"{"id":"us1","method":"read_ultrasonic","params":{}}"#;
        let resp_str = handler.dispatch(raw, 0).await;
        let resp: serde_json::Value = serde_json::from_str(&resp_str).unwrap();

        assert_eq!(resp["id"], "us1");
        assert_eq!(resp["ok"], false);
        assert_eq!(resp["error"]["code"], "TIMEOUT");
        assert!(
            resp["error"]["message"]
                .as_str()
                .unwrap_or("")
                .contains("1 ms"),
            "TIMEOUT message should include the timeout value"
        );
    }

    #[tokio::test]
    async fn read_ultrasonic_no_echo_returns_no_echo_error() {
        // ECHO goes high immediately then low after 2 reads → near-zero elapsed
        // time → distance ≈ 0 cm < 2 cm → driver returns NoEcho.
        let echo_bcm = Config::default().ultrasonic.echo_pin_bcm;
        let mock = MockUltrasonicGpio::new(echo_bcm, 2, 2);
        let handler = test_handler_with_ultrasonic(mock);

        let raw = r#"{"id":"us2","method":"read_ultrasonic","params":{}}"#;
        let resp_str = handler.dispatch(raw, 0).await;
        let resp: serde_json::Value = serde_json::from_str(&resp_str).unwrap();

        assert_eq!(resp["id"], "us2");
        assert_eq!(resp["ok"], false);
        assert_eq!(resp["error"]["code"], "NO_ECHO");
    }

    #[tokio::test]
    async fn read_ultrasonic_success_response_shape() {
        // When the driver returns Ok, the IPC response must contain distance_cm.
        // With a mock the elapsed time is near-zero so the driver most often
        // returns NoEcho; we accept both outcomes and validate each shape.
        let echo_bcm = Config::default().ultrasonic.echo_pin_bcm;
        let mock = MockUltrasonicGpio::new(echo_bcm, 1, 100);
        let handler = test_handler_with_ultrasonic(mock);

        let raw = r#"{"id":"us3","method":"read_ultrasonic","params":{}}"#;
        let resp_str = handler.dispatch(raw, 0).await;
        let resp: serde_json::Value = serde_json::from_str(&resp_str).unwrap();

        assert_eq!(resp["id"], "us3");
        if resp["ok"].as_bool().unwrap_or(false) {
            assert!(
                resp["result"]["distance_cm"].is_number(),
                "successful response must contain numeric distance_cm"
            );
        } else {
            // Sub-2 cm or out-of-range produces NO_ECHO with the zero-time mock.
            assert_eq!(resp["error"]["code"], "NO_ECHO");
        }
    }

    // ------------------------------------------------------------------
    // Speaker
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn enable_speaker_returns_enabled_true() {
        let handler = test_handler();
        let raw = r#"{"id":"sp1","method":"enable_speaker","params":{}}"#;
        let resp_str = handler.dispatch(raw, 0).await;
        let resp: serde_json::Value = serde_json::from_str(&resp_str).unwrap();

        assert_eq!(resp["id"], "sp1");
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["result"]["enabled"], true);
        assert_eq!(
            resp["result"]["pin_bcm"],
            handler.config().speaker_en_pin_bcm
        );
    }

    #[tokio::test]
    async fn disable_speaker_returns_enabled_false() {
        let handler = test_handler();
        let raw = r#"{"id":"sp2","method":"disable_speaker","params":{}}"#;
        let resp_str = handler.dispatch(raw, 0).await;
        let resp: serde_json::Value = serde_json::from_str(&resp_str).unwrap();

        assert_eq!(resp["id"], "sp2");
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["result"]["enabled"], false);
        assert_eq!(
            resp["result"]["pin_bcm"],
            handler.config().speaker_en_pin_bcm
        );
    }

    #[tokio::test]
    async fn enable_then_disable_speaker_toggles_pin() {
        let handler = test_handler();
        handler
            .dispatch(r#"{"id":"sp3","method":"enable_speaker","params":{}}"#, 0)
            .await;
        let bcm = handler.config().speaker_en_pin_bcm;
        // After enable, the speaker enable pin must be high (read via gpio mock).
        let pin_high = handler.gpio.bus.lock().await.read_pin(bcm).unwrap();
        assert!(
            pin_high,
            "speaker enable pin should be HIGH after enable_speaker"
        );

        handler
            .dispatch(r#"{"id":"sp4","method":"disable_speaker","params":{}}"#, 0)
            .await;
        let pin_high = handler.gpio.bus.lock().await.read_pin(bcm).unwrap();
        assert!(
            !pin_high,
            "speaker enable pin should be LOW after disable_speaker"
        );
    }

    // ------------------------------------------------------------------
    // Mock AlsaControl for audio level tests
    // ------------------------------------------------------------------

    fn test_handler_with_mock_alsa(mock: MockAlsaControl) -> Handler {
        let hat = Arc::new(Hat::new(MockI2c { response: [0, 0] }, 0x14));
        let gpio = Arc::new(HatGpio::new(MockGpio::new()));
        Handler::with_alsa(Arc::new(Config::default()), hat, gpio, Arc::new(mock))
    }

    // ------------------------------------------------------------------
    // set_volume / get_volume
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn set_volume_stores_percentage_and_returns_it() {
        let handler = test_handler_with_mock_alsa(MockAlsaControl::new(50, 50));
        let raw = r#"{"id":"v1","method":"set_volume","params":{"volume_pct":80}}"#;
        let resp: serde_json::Value =
            serde_json::from_str(&handler.dispatch(raw, 0).await).unwrap();
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["result"]["volume_pct"], 80);
    }

    #[tokio::test]
    async fn get_volume_returns_stored_percentage() {
        let handler = test_handler_with_mock_alsa(MockAlsaControl::new(65, 40));
        let raw = r#"{"id":"v2","method":"get_volume","params":{}}"#;
        let resp: serde_json::Value =
            serde_json::from_str(&handler.dispatch(raw, 0).await).unwrap();
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["result"]["volume_pct"], 65);
    }

    #[tokio::test]
    async fn set_volume_rejects_out_of_range() {
        let handler = test_handler_with_mock_alsa(MockAlsaControl::new(50, 50));
        let raw = r#"{"id":"v3","method":"set_volume","params":{"volume_pct":101}}"#;
        let resp: serde_json::Value =
            serde_json::from_str(&handler.dispatch(raw, 0).await).unwrap();
        assert_eq!(resp["ok"], false);
        assert_eq!(resp["error"]["code"], "INVALID_PARAMS");
    }

    #[tokio::test]
    async fn set_volume_missing_param_returns_invalid_params() {
        let handler = test_handler_with_mock_alsa(MockAlsaControl::new(50, 50));
        let raw = r#"{"id":"v4","method":"set_volume","params":{}}"#;
        let resp: serde_json::Value =
            serde_json::from_str(&handler.dispatch(raw, 0).await).unwrap();
        assert_eq!(resp["ok"], false);
        assert_eq!(resp["error"]["code"], "INVALID_PARAMS");
    }

    #[tokio::test]
    async fn set_volume_hardware_error_returns_hardware_error_code() {
        let handler = test_handler_with_mock_alsa(MockAlsaControl::failing());
        let raw = r#"{"id":"v5","method":"set_volume","params":{"volume_pct":50}}"#;
        let resp: serde_json::Value =
            serde_json::from_str(&handler.dispatch(raw, 0).await).unwrap();
        assert_eq!(resp["ok"], false);
        assert_eq!(resp["error"]["code"], "HARDWARE_ERROR");
    }

    #[tokio::test]
    async fn get_volume_hardware_error_returns_hardware_error_code() {
        let handler = test_handler_with_mock_alsa(MockAlsaControl::failing());
        let raw = r#"{"id":"v6","method":"get_volume","params":{}}"#;
        let resp: serde_json::Value =
            serde_json::from_str(&handler.dispatch(raw, 0).await).unwrap();
        assert_eq!(resp["ok"], false);
        assert_eq!(resp["error"]["code"], "HARDWARE_ERROR");
    }

    // ------------------------------------------------------------------
    // set_mic_gain / get_mic_gain
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn set_mic_gain_stores_percentage_and_returns_it() {
        let handler = test_handler_with_mock_alsa(MockAlsaControl::new(50, 50));
        let raw = r#"{"id":"g1","method":"set_mic_gain","params":{"gain_pct":70}}"#;
        let resp: serde_json::Value =
            serde_json::from_str(&handler.dispatch(raw, 0).await).unwrap();
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["result"]["gain_pct"], 70);
    }

    #[tokio::test]
    async fn get_mic_gain_returns_stored_percentage() {
        let handler = test_handler_with_mock_alsa(MockAlsaControl::new(40, 35));
        let raw = r#"{"id":"g2","method":"get_mic_gain","params":{}}"#;
        let resp: serde_json::Value =
            serde_json::from_str(&handler.dispatch(raw, 0).await).unwrap();
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["result"]["gain_pct"], 35);
    }

    #[tokio::test]
    async fn set_mic_gain_rejects_out_of_range() {
        let handler = test_handler_with_mock_alsa(MockAlsaControl::new(50, 50));
        let raw = r#"{"id":"g3","method":"set_mic_gain","params":{"gain_pct":200}}"#;
        let resp: serde_json::Value =
            serde_json::from_str(&handler.dispatch(raw, 0).await).unwrap();
        assert_eq!(resp["ok"], false);
        assert_eq!(resp["error"]["code"], "INVALID_PARAMS");
    }

    #[tokio::test]
    async fn set_mic_gain_missing_param_returns_invalid_params() {
        let handler = test_handler_with_mock_alsa(MockAlsaControl::new(50, 50));
        let raw = r#"{"id":"g4","method":"set_mic_gain","params":{}}"#;
        let resp: serde_json::Value =
            serde_json::from_str(&handler.dispatch(raw, 0).await).unwrap();
        assert_eq!(resp["ok"], false);
        assert_eq!(resp["error"]["code"], "INVALID_PARAMS");
    }

    #[tokio::test]
    async fn set_mic_gain_hardware_error_returns_hardware_error_code() {
        let handler = test_handler_with_mock_alsa(MockAlsaControl::failing());
        let raw = r#"{"id":"g5","method":"set_mic_gain","params":{"gain_pct":50}}"#;
        let resp: serde_json::Value =
            serde_json::from_str(&handler.dispatch(raw, 0).await).unwrap();
        assert_eq!(resp["ok"], false);
        assert_eq!(resp["error"]["code"], "HARDWARE_ERROR");
    }

    #[tokio::test]
    async fn get_mic_gain_hardware_error_returns_hardware_error_code() {
        let handler = test_handler_with_mock_alsa(MockAlsaControl::failing());
        let raw = r#"{"id":"g6","method":"get_mic_gain","params":{}}"#;
        let resp: serde_json::Value =
            serde_json::from_str(&handler.dispatch(raw, 0).await).unwrap();
        assert_eq!(resp["ok"], false);
        assert_eq!(resp["error"]["code"], "HARDWARE_ERROR");
    }

    // ------------------------------------------------------------------
    // Calibration handler tests
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn get_calibration_returns_defaults() {
        let handler = test_handler();
        let raw = r#"{"id":"c1","method":"get_calibration","params":{}}"#;
        let resp: serde_json::Value =
            serde_json::from_str(&handler.dispatch(raw, 0).await).unwrap();
        assert_eq!(resp["ok"], true);
        let motors = &resp["result"]["motors"];
        assert!(motors.is_array());
        assert_eq!(motors[0]["speed_scale"], 1.0);
        assert_eq!(motors[0]["deadband_pct"], 0.0);
        assert_eq!(motors[0]["reversed"], false);
        let servos = &resp["result"]["servos"];
        assert_eq!(servos["steering"]["trim_us"], 0);
        let grayscale = &resp["result"]["grayscale"];
        assert_eq!(grayscale[0]["white_raw"], 100);
        assert_eq!(grayscale[0]["black_raw"], 3000);
    }

    #[tokio::test]
    async fn set_motor_calibration_partial_update_only_changes_speed_scale() {
        let handler = test_handler();
        // Set a full entry first.
        handler
            .dispatch(
                r#"{"id":"c2a","method":"set_motor_calibration","params":{"channel":0,"speed_scale":1.5,"deadband_pct":5.0,"reversed":true}}"#,
                0,
            )
            .await;
        // Partial update — only speed_scale.
        let raw = r#"{"id":"c2b","method":"set_motor_calibration","params":{"channel":0,"speed_scale":1.2}}"#;
        let resp: serde_json::Value =
            serde_json::from_str(&handler.dispatch(raw, 0).await).unwrap();
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["result"]["speed_scale"], 1.2);
        // deadband_pct and reversed should be preserved from the earlier set.
        assert_eq!(resp["result"]["deadband_pct"], 5.0);
        assert_eq!(resp["result"]["reversed"], true);
    }

    #[tokio::test]
    async fn set_motor_calibration_invalid_channel_returns_invalid_params() {
        let handler = test_handler();
        let raw = r#"{"id":"c3","method":"set_motor_calibration","params":{"channel":99,"speed_scale":1.0}}"#;
        let resp: serde_json::Value =
            serde_json::from_str(&handler.dispatch(raw, 0).await).unwrap();
        assert_eq!(resp["ok"], false);
        assert_eq!(resp["error"]["code"], "INVALID_PARAMS");
    }

    #[tokio::test]
    async fn set_motor_calibration_rejects_out_of_range_speed_scale() {
        let handler = test_handler();
        let raw = r#"{"id":"c4","method":"set_motor_calibration","params":{"channel":0,"speed_scale":0.1}}"#;
        let resp: serde_json::Value =
            serde_json::from_str(&handler.dispatch(raw, 0).await).unwrap();
        assert_eq!(resp["ok"], false);
        assert_eq!(resp["error"]["code"], "INVALID_PARAMS");
    }

    #[tokio::test]
    async fn set_motor_calibration_rejects_out_of_range_deadband_pct() {
        let handler = test_handler();
        // Below lower bound (negative).
        let raw = r#"{"id":"c_db1","method":"set_motor_calibration","params":{"channel":0,"deadband_pct":-1.0}}"#;
        let resp: serde_json::Value =
            serde_json::from_str(&handler.dispatch(raw, 0).await).unwrap();
        assert_eq!(resp["ok"], false);
        assert_eq!(resp["error"]["code"], "INVALID_PARAMS");
        // Above upper bound.
        let raw = r#"{"id":"c_db2","method":"set_motor_calibration","params":{"channel":0,"deadband_pct":21.0}}"#;
        let resp: serde_json::Value =
            serde_json::from_str(&handler.dispatch(raw, 0).await).unwrap();
        assert_eq!(resp["ok"], false);
        assert_eq!(resp["error"]["code"], "INVALID_PARAMS");
    }

    #[tokio::test]
    async fn set_servo_calibration_valid_steering_trim() {
        let handler = test_handler();
        let raw = r#"{"id":"c5","method":"set_servo_calibration","params":{"servo":"steering","trim_us":-50}}"#;
        let resp: serde_json::Value =
            serde_json::from_str(&handler.dispatch(raw, 0).await).unwrap();
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["result"]["servo"], "steering");
        assert_eq!(resp["result"]["trim_us"], -50);
    }

    #[tokio::test]
    async fn set_servo_calibration_invalid_name_returns_invalid_params() {
        let handler = test_handler();
        let raw = r#"{"id":"c6","method":"set_servo_calibration","params":{"servo":"unknown_servo","trim_us":0}}"#;
        let resp: serde_json::Value =
            serde_json::from_str(&handler.dispatch(raw, 0).await).unwrap();
        assert_eq!(resp["ok"], false);
        assert_eq!(resp["error"]["code"], "INVALID_PARAMS");
    }

    #[tokio::test]
    async fn set_servo_calibration_rejects_out_of_range_trim_us() {
        let handler = test_handler();
        // Above upper bound (+501).
        let raw = r#"{"id":"c_trim1","method":"set_servo_calibration","params":{"servo":"steering","trim_us":501}}"#;
        let resp: serde_json::Value =
            serde_json::from_str(&handler.dispatch(raw, 0).await).unwrap();
        assert_eq!(resp["ok"], false);
        assert_eq!(resp["error"]["code"], "INVALID_PARAMS");
        // Below lower bound (-501).
        let raw = r#"{"id":"c_trim2","method":"set_servo_calibration","params":{"servo":"steering","trim_us":-501}}"#;
        let resp: serde_json::Value =
            serde_json::from_str(&handler.dispatch(raw, 0).await).unwrap();
        assert_eq!(resp["ok"], false);
        assert_eq!(resp["error"]["code"], "INVALID_PARAMS");
    }

    #[tokio::test]
    async fn calibrate_grayscale_white_capture_stores_value() {
        // ADC returns [0, 0] → raw = 0, well below default black_raw=3000.
        let handler = test_handler();
        let raw = r#"{"id":"c7","method":"calibrate_grayscale","params":{"channel":0,"surface":"white"}}"#;
        let resp: serde_json::Value =
            serde_json::from_str(&handler.dispatch(raw, 0).await).unwrap();
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["result"]["channel"], 0);
        assert_eq!(resp["result"]["surface"], "white");
        assert_eq!(resp["result"]["stored"], true);
    }

    #[tokio::test]
    async fn calibrate_grayscale_black_capture_stores_value() {
        // ADC returns [0x0F, 0xFF] → raw = 4095, well above default white_raw=100.
        let handler = test_handler_with_adc(0x0F, 0xFF);
        let raw = r#"{"id":"c8","method":"calibrate_grayscale","params":{"channel":1,"surface":"black"}}"#;
        let resp: serde_json::Value =
            serde_json::from_str(&handler.dispatch(raw, 0).await).unwrap();
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["result"]["channel"], 1);
        assert_eq!(resp["result"]["surface"], "black");
        assert_eq!(resp["result"]["stored"], true);
    }

    #[tokio::test]
    async fn calibrate_grayscale_constraint_violation_returns_invalid_params() {
        // ADC returns [0xFF, 0xFF] → raw = 65535.
        // Trying to store this as white_raw (65535 >= black_raw=3000) violates constraint.
        let handler = test_handler_with_adc(0xFF, 0xFF);
        let raw = r#"{"id":"c9","method":"calibrate_grayscale","params":{"channel":0,"surface":"white"}}"#;
        let resp: serde_json::Value =
            serde_json::from_str(&handler.dispatch(raw, 0).await).unwrap();
        assert_eq!(resp["ok"], false);
        assert_eq!(resp["error"]["code"], "INVALID_PARAMS");
    }

    #[tokio::test]
    async fn calibrate_grayscale_invalid_channel_returns_invalid_params() {
        let handler = test_handler();
        let raw = r#"{"id":"c_gs_inv","method":"calibrate_grayscale","params":{"channel":3,"surface":"white"}}"#;
        let resp: serde_json::Value =
            serde_json::from_str(&handler.dispatch(raw, 0).await).unwrap();
        assert_eq!(resp["ok"], false);
        assert_eq!(resp["error"]["code"], "INVALID_PARAMS");
    }

    #[tokio::test]
    async fn save_calibration_writes_to_configured_path() {
        let dir = tempfile::tempdir().unwrap();
        let cal_path = dir.path().join("calibration.toml");
        let handler = test_handler_with_config(Arc::new(Config {
            calibration_path: cal_path.clone(),
            ..Default::default()
        }));

        let raw = r#"{"id":"c10","method":"save_calibration","params":{}}"#;
        let resp: serde_json::Value =
            serde_json::from_str(&handler.dispatch(raw, 0).await).unwrap();
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["result"]["saved"], true);
        assert!(cal_path.exists(), "calibration file must exist after save");
    }

    #[tokio::test]
    async fn reset_calibration_reverts_modified_store() {
        let handler = test_handler();
        // Apply a non-default value.
        handler
            .dispatch(
                r#"{"id":"r1","method":"set_motor_calibration","params":{"channel":0,"speed_scale":1.8}}"#,
                0,
            )
            .await;
        // Reset.
        let raw = r#"{"id":"r2","method":"reset_calibration","params":{}}"#;
        let resp: serde_json::Value =
            serde_json::from_str(&handler.dispatch(raw, 0).await).unwrap();
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["result"]["reset"], true);

        // Verify reset to 1.0.
        let snap: serde_json::Value = serde_json::from_str(
            &handler
                .dispatch(r#"{"id":"r3","method":"get_calibration","params":{}}"#, 0)
                .await,
        )
        .unwrap();
        assert_eq!(snap["result"]["motors"][0]["speed_scale"], 1.0);
    }

    #[tokio::test]
    async fn read_grayscale_normalized_returns_clamped_zero_for_raw_below_white() {
        // ADC returns [0, 0] → raw=0; white_raw=100; normalized = clamp((0-100)/(3000-100), 0,1) = 0.0
        let handler = test_handler();
        let raw = r#"{"id":"n1","method":"read_grayscale_normalized","params":{}}"#;
        let resp: serde_json::Value =
            serde_json::from_str(&handler.dispatch(raw, 0).await).unwrap();
        assert_eq!(resp["ok"], true);
        let normalized = &resp["result"]["normalized"];
        assert!(normalized.is_array());
        for n in normalized.as_array().unwrap() {
            assert_eq!(*n, serde_json::json!(0.0));
        }
    }

    #[tokio::test]
    async fn read_grayscale_normalized_mid_range_value() {
        // ADC returns [0x08, 0x98] = 2200. With defaults white=100, black=3000:
        // normalized = (2200 - 100) / (3000 - 100) = 2100/2900 ≈ 0.7241
        let handler = test_handler_with_adc(0x08, 0x98);
        let raw = r#"{"id":"n2","method":"read_grayscale_normalized","params":{}}"#;
        let resp: serde_json::Value =
            serde_json::from_str(&handler.dispatch(raw, 0).await).unwrap();
        assert_eq!(resp["ok"], true);
        let n0 = resp["result"]["normalized"][0].as_f64().unwrap();
        let expected = (2200.0_f64 - 100.0) / (3000.0 - 100.0);
        assert!(
            (n0 - expected).abs() < 0.005,
            "expected ≈{expected:.4}, got {n0}"
        );
    }

    // -----------------------------------------------------------------------
    // Routine handler tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn start_routine_returns_started() {
        let handler = test_handler();
        let raw = r#"{"id":"rt1","method":"start_routine","params":{"name":"explore"}}"#;
        let resp: serde_json::Value =
            serde_json::from_str(&handler.dispatch(raw, 0).await).unwrap();
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["result"]["name"], "explore");
        assert!(resp["result"]["started_at_uptime_s"].is_number());
    }

    #[tokio::test]
    async fn start_routine_unknown_name_returns_invalid_params() {
        let handler = test_handler();
        let raw = r#"{"id":"rt2","method":"start_routine","params":{"name":"fly"}}"#;
        let resp: serde_json::Value =
            serde_json::from_str(&handler.dispatch(raw, 0).await).unwrap();
        assert_eq!(resp["ok"], false);
        assert_eq!(resp["error"]["code"], "INVALID_PARAMS");
    }

    #[tokio::test]
    async fn start_routine_already_running_returns_already_running() {
        let handler = test_handler();
        let raw = r#"{"id":"rt3a","method":"start_routine","params":{"name":"explore"}}"#;
        handler.dispatch(raw, 0).await;
        let raw2 = r#"{"id":"rt3b","method":"start_routine","params":{"name":"explore"}}"#;
        let resp: serde_json::Value =
            serde_json::from_str(&handler.dispatch(raw2, 0).await).unwrap();
        assert_eq!(resp["ok"], false);
        assert_eq!(resp["error"]["code"], "ALREADY_RUNNING");
    }

    #[tokio::test]
    async fn stop_routine_returns_stats() {
        let handler = test_handler();
        let start_raw = r#"{"id":"rt4a","method":"start_routine","params":{"name":"explore"}}"#;
        handler.dispatch(start_raw, 0).await;
        let stop_raw = r#"{"id":"rt4b","method":"stop_routine","params":{}}"#;
        let resp: serde_json::Value =
            serde_json::from_str(&handler.dispatch(stop_raw, 0).await).unwrap();
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["result"]["name"], "explore");
        assert!(resp["result"]["ran_for_s"].is_number());
        assert!(resp["result"]["obstacles_avoided"].is_number());
        assert!(resp["result"]["cliffs_avoided"].is_number());
        assert!(resp["result"]["stop_reason"].is_string());
    }

    #[tokio::test]
    async fn stop_routine_not_running_returns_invalid_params() {
        let handler = test_handler();
        let raw = r#"{"id":"rt5","method":"stop_routine","params":{}}"#;
        let resp: serde_json::Value =
            serde_json::from_str(&handler.dispatch(raw, 0).await).unwrap();
        assert_eq!(resp["ok"], false);
        assert_eq!(resp["error"]["code"], "INVALID_PARAMS");
    }

    #[tokio::test]
    async fn get_routine_status_idle_returns_not_running() {
        let handler = test_handler();
        let raw = r#"{"id":"rt6","method":"get_routine_status","params":{}}"#;
        let resp: serde_json::Value =
            serde_json::from_str(&handler.dispatch(raw, 0).await).unwrap();
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["result"]["running"], false);
        assert!(resp["result"]["name"].is_null());
        assert!(resp["result"]["elapsed_s"].is_null());
    }

    #[tokio::test]
    async fn get_routine_status_running_returns_running() {
        let handler = test_handler();
        let start_raw = r#"{"id":"rt7a","method":"start_routine","params":{"name":"explore"}}"#;
        handler.dispatch(start_raw, 0).await;
        let raw = r#"{"id":"rt7b","method":"get_routine_status","params":{}}"#;
        let resp: serde_json::Value =
            serde_json::from_str(&handler.dispatch(raw, 0).await).unwrap();
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["result"]["running"], true);
        assert_eq!(resp["result"]["name"], "explore");
        assert!(resp["result"]["elapsed_s"].is_number());
    }
}
