// Configuration: CLI args → env vars → config file (priority order).

use std::path::{Path, PathBuf};
use std::{env, fs};

use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config file {path}: {source}")]
    ReadFile {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse config file {path}: {source}")]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("invalid {field}: {reason}")]
    Validation { field: &'static str, reason: String },
}

/// Configuration for a single motor channel.
///
/// Each entry maps an IPC motor index (position in the `motors` array, 0-based)
/// to a Robot HAT V4 PWM channel and a BCM GPIO direction pin.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct MotorConfig {
    /// HAT PWM channel for motor speed control (12–15).
    pub pwm_channel: u8,
    /// BCM GPIO pin number for motor direction (HIGH = forward, LOW = backward).
    pub dir_pin_bcm: u8,
    /// Invert the direction signal for this motor (wired with reversed polarity).
    #[serde(default)]
    pub reversed: bool,
}

/// Named servo channel assignments for PicarX (configurable).
///
/// Each field maps a semantic servo name to a PWM channel number (0–11).
/// Set to `None` to disable that servo for this robot configuration.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ServoChannels {
    /// Camera pan servo (horizontal, left/right). Default: channel 0.
    pub camera_pan: Option<u8>,
    /// Camera tilt servo (vertical, up/down). Default: channel 1.
    pub camera_tilt: Option<u8>,
    /// Front-wheel steering servo. Default: channel 2.
    pub steering: Option<u8>,
}

impl Default for ServoChannels {
    fn default() -> Self {
        Self {
            camera_pan: Some(0),
            camera_tilt: Some(1),
            steering: Some(2),
        }
    }
}

/// Named ADC sensor channel assignments.
///
/// ADC channels A0–A7 on the Robot HAT V4.  The three grayscale sensors
/// (line / cliff detection) use channels A0–A2 on PicarX.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct SensorChannels {
    /// Grayscale sensor ADC channels [left, center, right]. Default: [0, 1, 2].
    pub grayscale: [u8; 3],
}

impl Default for SensorChannels {
    fn default() -> Self {
        Self {
            grayscale: [0, 1, 2],
        }
    }
}

/// ALSA mixer configuration for audio output (speaker) and input
/// (USB microphone PCM2902).
///
/// Cards are resolved by *name* against `/proc/asound/cards` (an explicit
/// numeric index overrides): ALSA card numbers depend on driver load order,
/// and a stale hardcoded index silently targets the wrong card — the robot
/// has only the playback-only HDMI card (0) and the USB codec (1), and the
/// Robot HAT speaker's I2S DAC only exists once `dtoverlay=hifiberry-dac`
/// is enabled in the Pi boot config.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default)]
pub struct AudioConfig {
    /// Explicit ALSA card index for speaker output; None = resolve by name.
    pub output_card_index: Option<u8>,
    /// Case-insensitive substring matched against `/proc/asound/cards` to
    /// find the output card (default: "sndrpihifiberry", the Robot HAT I2S
    /// DAC card id).  No match falls back to the ALSA default card.
    pub output_card_name: String,
    /// ALSA mixer control name for output volume (default: "Master"; the
    /// I2S DAC has no hardware mixer, so this must match the softvol
    /// control configured in asound.conf).
    pub output_control: String,
    /// Explicit ALSA card index for the microphone; None = resolve by name.
    pub input_card_index: Option<u8>,
    /// Case-insensitive substring matched against `/proc/asound/cards` to
    /// find the input card (default: "USB", the PCM2902 codec).
    pub input_card_name: String,
    /// ALSA mixer capture controls driven for microphone gain, in priority
    /// order (default: ["Mic", "Capture"]). Each control that exists on the
    /// card is set; absent ones are skipped. USB codecs differ in whether the
    /// ADC level is named "Mic" or "Capture", so both are driven by default.
    pub input_controls: Vec<String>,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            output_card_index: None,
            output_card_name: "sndrpihifiberry".into(),
            output_control: "Master".into(),
            input_card_index: None,
            input_card_name: "USB".into(),
            input_controls: vec!["Mic".into(), "Capture".into()],
        }
    }
}

/// Ultrasonic distance sensor GPIO pin assignments.
///
/// The HC-SR04-compatible sensor on PicarX uses D2 (BCM 27) for TRIG and
/// D3 (BCM 22) for ECHO.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct UltrasonicConfig {
    /// BCM GPIO pin for the TRIG output. Default: 27 (D2).
    pub trig_pin_bcm: u8,
    /// BCM GPIO pin for the ECHO input. Default: 22 (D3).
    pub echo_pin_bcm: u8,
    /// Measurement timeout in milliseconds. Default: 20.
    pub timeout_ms: u64,
}

impl Default for UltrasonicConfig {
    fn default() -> Self {
        Self {
            trig_pin_bcm: 27,
            echo_pin_bcm: 22,
            timeout_ms: 20,
        }
    }
}

/// Configuration for autonomous on-robot routines.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct RoutineConfig {
    /// Forward speed percentage for the explore routine (1.0–100.0).
    pub explore_speed_pct: f64,
    /// Obstacle detection threshold in centimetres (must be > 0).
    pub obstacle_threshold_cm: f64,
    /// Cliff detection threshold as a normalised grayscale value (0.0–1.0).
    pub cliff_threshold_normalized: f64,
    /// Delay between sensor-poll iterations in milliseconds (≥ 50).
    pub loop_interval_ms: u64,
    /// Duration to reverse during avoidance manoeuvre in milliseconds.
    pub avoidance_backup_ms: u64,
    /// Angle added to 90° for the avoidance turn (degrees).
    pub avoidance_turn_angle_deg: f64,
    /// Maximum routine runtime before automatic stop (seconds).
    pub max_duration_s: u64,
}

impl Default for RoutineConfig {
    fn default() -> Self {
        Self {
            explore_speed_pct: 30.0,
            obstacle_threshold_cm: 25.0,
            cliff_threshold_normalized: 0.7,
            loop_interval_ms: 100,
            avoidance_backup_ms: 500,
            avoidance_turn_angle_deg: 60.0,
            max_duration_s: 300,
        }
    }
}

/// Daemon configuration.
///
/// Loaded from TOML file, overridden by `NOMON_HAT_*` environment variables.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    pub i2c_bus: u8,
    pub hat_address: u8,
    pub socket_path: PathBuf,
    pub socket_mode: u32,
    pub log_level: String,
    pub servo_default_ttl_ms: u64,
    pub motor_default_ttl_ms: u64,
    pub watchdog_poll_ms: u64,
    /// Motor channel configurations (up to 4).  Positions in the Vec are the
    /// IPC motor indices (0-based) used in `set_motor_speed` requests.
    pub motors: Vec<MotorConfig>,
    /// Named servo channel assignments.
    pub servos: ServoChannels,
    /// Named ADC sensor channel assignments.
    pub sensors: SensorChannels,
    /// Ultrasonic distance sensor GPIO pin assignments.
    pub ultrasonic: UltrasonicConfig,
    /// BCM GPIO pin for the speaker amplifier enable signal. Default: 20.
    pub speaker_en_pin_bcm: u8,
    /// ALSA audio mixer configuration (output volume + microphone gain).
    pub audio: AudioConfig,
    /// Filesystem path for the calibration TOML file.
    ///
    /// Loaded at startup; written by `save_calibration`. Override with the
    /// `NOMON_HAT_CALIBRATION_PATH` environment variable.
    pub calibration_path: PathBuf,
    /// Autonomous routine configuration.
    #[serde(default)]
    pub routine: RoutineConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            i2c_bus: 1,
            hat_address: 0x14,
            socket_path: PathBuf::from("/run/nomopractic/nomopractic.sock"),
            socket_mode: 0o660,
            log_level: "info".into(),
            servo_default_ttl_ms: 500,
            motor_default_ttl_ms: 500,
            watchdog_poll_ms: 100,
            // PicarX default wiring: motor 0 = P12/D5, motor 1 = P13/D4.
            // Motor 1 is physically mounted with reversed polarity on PicarX,
            // so positive speed_pct corresponds to forward for both motors.
            motors: vec![
                MotorConfig {
                    pwm_channel: 12,
                    dir_pin_bcm: 24, // D5
                    reversed: false,
                },
                MotorConfig {
                    pwm_channel: 13,
                    dir_pin_bcm: 23, // D4
                    reversed: true,
                },
            ],
            servos: ServoChannels::default(),
            sensors: SensorChannels::default(),
            ultrasonic: UltrasonicConfig::default(),
            speaker_en_pin_bcm: 20,
            audio: AudioConfig::default(),
            calibration_path: PathBuf::from("/etc/nomopractic/calibration.toml"),
            routine: RoutineConfig::default(),
        }
    }
}

impl Config {
    /// Load configuration from a TOML file, then apply environment variable
    /// overrides. Returns compiled defaults if the file does not exist.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let mut config = if path.exists() {
            let contents = fs::read_to_string(path).map_err(|e| ConfigError::ReadFile {
                path: path.to_owned(),
                source: e,
            })?;
            toml::from_str(&contents).map_err(|e| ConfigError::Parse {
                path: path.to_owned(),
                source: e,
            })?
        } else {
            Self::default()
        };

        config.apply_env_overrides(|k| env::var(k));
        config.validate()?;
        Ok(config)
    }

    /// Override fields from `NOMON_HAT_*` environment variables.
    ///
    /// Accepts a `get_env` function so callers can inject a deterministic
    /// accessor in tests instead of mutating the process environment.
    fn apply_env_overrides<F>(&mut self, get_env: F)
    where
        F: for<'a> Fn(&'a str) -> Result<String, env::VarError>,
    {
        if let Ok(v) = get_env("NOMON_HAT_I2C_BUS")
            && let Ok(n) = v.parse()
        {
            self.i2c_bus = n;
        }
        if let Ok(v) = get_env("NOMON_HAT_ADDRESS") {
            // Accept "0x14" or "20" decimal.
            let n = if let Some(hex) = v.strip_prefix("0x") {
                u8::from_str_radix(hex, 16).ok()
            } else {
                v.parse().ok()
            };
            if let Some(n) = n {
                self.hat_address = n;
            }
        }
        if let Ok(v) = get_env("NOMON_HAT_SOCKET_PATH") {
            self.socket_path = PathBuf::from(v);
        }
        if let Ok(v) = get_env("NOMON_HAT_SOCKET_MODE")
            && let Ok(n) = u32::from_str_radix(&v, 8)
        {
            self.socket_mode = n;
        }
        if let Ok(v) = get_env("NOMON_HAT_LOG_LEVEL") {
            self.log_level = v;
        }
        if let Ok(v) = get_env("NOMON_HAT_SERVO_DEFAULT_TTL_MS")
            && let Ok(n) = v.parse()
        {
            self.servo_default_ttl_ms = n;
        }
        if let Ok(v) = get_env("NOMON_HAT_MOTOR_DEFAULT_TTL_MS")
            && let Ok(n) = v.parse()
        {
            self.motor_default_ttl_ms = n;
        }
        if let Ok(v) = get_env("NOMON_HAT_WATCHDOG_POLL_MS")
            && let Ok(n) = v.parse()
        {
            self.watchdog_poll_ms = n;
        }
        if let Ok(v) = get_env("NOMON_HAT_CALIBRATION_PATH") {
            self.calibration_path = PathBuf::from(v);
        }
    }

    /// Validate configuration values.
    fn validate(&self) -> Result<(), ConfigError> {
        const VALID_LOG_LEVELS: &[&str] = &["trace", "debug", "info", "warn", "error"];
        if !VALID_LOG_LEVELS.contains(&self.log_level.to_lowercase().as_str()) {
            return Err(ConfigError::Validation {
                field: "log_level",
                reason: format!("'{}' is not one of {:?}", self.log_level, VALID_LOG_LEVELS),
            });
        }
        if self.servo_default_ttl_ms == 0 {
            return Err(ConfigError::Validation {
                field: "servo_default_ttl_ms",
                reason: "must be > 0".into(),
            });
        }
        if self.motor_default_ttl_ms == 0 {
            return Err(ConfigError::Validation {
                field: "motor_default_ttl_ms",
                reason: "must be > 0".into(),
            });
        }
        if self.motors.len() > 4 {
            return Err(ConfigError::Validation {
                field: "motors",
                reason: "at most 4 motor channels are supported".into(),
            });
        }
        for (i, m) in self.motors.iter().enumerate() {
            if !(12..=15).contains(&m.pwm_channel) {
                return Err(ConfigError::Validation {
                    field: "motors[].pwm_channel",
                    reason: format!(
                        "motors[{i}].pwm_channel {} is out of range 12–15",
                        m.pwm_channel
                    ),
                });
            }
        }
        // Validate uniqueness of pwm_channel and dir_pin_bcm across all motors.
        let mut seen_pwm: std::collections::HashSet<u8> = std::collections::HashSet::new();
        let mut seen_dir: std::collections::HashSet<u8> = std::collections::HashSet::new();
        for (i, m) in self.motors.iter().enumerate() {
            if !seen_pwm.insert(m.pwm_channel) {
                return Err(ConfigError::Validation {
                    field: "motors[].pwm_channel",
                    reason: format!(
                        "motors[{i}].pwm_channel {} is duplicated; each motor must use a unique PWM channel",
                        m.pwm_channel
                    ),
                });
            }
            if !seen_dir.insert(m.dir_pin_bcm) {
                return Err(ConfigError::Validation {
                    field: "motors[].dir_pin_bcm",
                    reason: format!(
                        "motors[{i}].dir_pin_bcm {} is duplicated; each motor must use a unique direction pin",
                        m.dir_pin_bcm
                    ),
                });
            }
        }
        // Validate named servo channels.
        for (name, ch) in [
            ("servos.camera_pan", self.servos.camera_pan),
            ("servos.camera_tilt", self.servos.camera_tilt),
            ("servos.steering", self.servos.steering),
        ] {
            if let Some(ch) = ch
                && ch > 11
            {
                return Err(ConfigError::Validation {
                    field: "servos",
                    reason: format!("{name} channel {ch} is out of range 0–11"),
                });
            }
        }
        // Validate grayscale sensor ADC channels.
        for (i, &ch) in self.sensors.grayscale.iter().enumerate() {
            if ch > 7 {
                return Err(ConfigError::Validation {
                    field: "sensors.grayscale",
                    reason: format!("sensors.grayscale[{i}] channel {ch} is out of range 0–7"),
                });
            }
        }
        if self.watchdog_poll_ms == 0 {
            return Err(ConfigError::Validation {
                field: "watchdog_poll_ms",
                reason: "must be > 0".into(),
            });
        }
        // Validate routine configuration.
        if !(1.0..=100.0).contains(&self.routine.explore_speed_pct) {
            return Err(ConfigError::Validation {
                field: "routine.explore_speed_pct",
                reason: format!(
                    "{} is out of range 1.0–100.0",
                    self.routine.explore_speed_pct
                ),
            });
        }
        if self.routine.obstacle_threshold_cm <= 0.0 {
            return Err(ConfigError::Validation {
                field: "routine.obstacle_threshold_cm",
                reason: "must be > 0.0".into(),
            });
        }
        if !(0.0..=1.0).contains(&self.routine.cliff_threshold_normalized) {
            return Err(ConfigError::Validation {
                field: "routine.cliff_threshold_normalized",
                reason: format!(
                    "{} is out of range 0.0–1.0",
                    self.routine.cliff_threshold_normalized
                ),
            });
        }
        if self.routine.loop_interval_ms < 50 {
            return Err(ConfigError::Validation {
                field: "routine.loop_interval_ms",
                reason: format!("{} is below minimum 50 ms", self.routine.loop_interval_ms),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn defaults_are_valid() {
        let config = Config::default();
        assert_eq!(config.i2c_bus, 1);
        assert_eq!(config.hat_address, 0x14);
        assert_eq!(config.socket_mode, 0o660);
        assert_eq!(config.log_level, "info");
        assert_eq!(config.servo_default_ttl_ms, 500);
        assert_eq!(config.motor_default_ttl_ms, 500);
        assert_eq!(config.watchdog_poll_ms, 100);
        assert_eq!(config.motors.len(), 2);
        assert_eq!(config.motors[0].pwm_channel, 12);
        assert_eq!(config.motors[1].pwm_channel, 13);
    }

    #[test]
    fn load_missing_file_returns_defaults() {
        let config = Config::load(Path::new("/nonexistent/config.toml")).unwrap();
        assert_eq!(config, Config::default());
    }

    #[test]
    fn load_toml_file() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(
            f,
            r#"
i2c_bus = 2
hat_address = 0x15
log_level = "debug"
servo_default_ttl_ms = 1000
"#
        )
        .unwrap();

        let config = Config::load(f.path()).unwrap();
        assert_eq!(config.i2c_bus, 2);
        assert_eq!(config.hat_address, 0x15);
        assert_eq!(config.log_level, "debug");
        assert_eq!(config.servo_default_ttl_ms, 1000);
        // Non-specified fields keep defaults.
        assert_eq!(config.watchdog_poll_ms, 100);
    }

    #[test]
    fn load_empty_file_returns_defaults() {
        let f = NamedTempFile::new().unwrap();
        let config = Config::load(f.path()).unwrap();
        assert_eq!(config, Config::default());
    }

    #[test]
    fn invalid_log_level_rejected() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, r#"log_level = "verbose""#).unwrap();
        let err = Config::load(f.path()).unwrap_err();
        assert!(err.to_string().contains("log_level"));
    }

    #[test]
    fn zero_ttl_rejected() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "servo_default_ttl_ms = 0").unwrap();
        let err = Config::load(f.path()).unwrap_err();
        assert!(err.to_string().contains("servo_default_ttl_ms"));
    }

    #[test]
    fn zero_watchdog_rejected() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "watchdog_poll_ms = 0").unwrap();
        let err = Config::load(f.path()).unwrap_err();
        assert!(err.to_string().contains("watchdog_poll_ms"));
    }

    #[test]
    fn zero_motor_ttl_rejected() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "motor_default_ttl_ms = 0").unwrap();
        let err = Config::load(f.path()).unwrap_err();
        assert!(err.to_string().contains("motor_default_ttl_ms"));
    }

    #[test]
    fn invalid_motor_pwm_channel_rejected() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(
            f,
            "[[motors]]\npwm_channel = 5\ndir_pin_bcm = 24\nreversed = false"
        )
        .unwrap();
        let err = Config::load(f.path()).unwrap_err();
        assert!(err.to_string().contains("pwm_channel"));
    }

    #[test]
    fn too_many_motors_rejected() {
        let mut f = NamedTempFile::new().unwrap();
        for ch in 12u8..=16 {
            writeln!(f, "[[motors]]\npwm_channel = {ch}\ndir_pin_bcm = 24").unwrap();
        }
        let err = Config::load(f.path()).unwrap_err();
        assert!(err.to_string().contains("motors"));
    }

    #[test]
    fn load_motor_config_from_toml() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(
            f,
            r#"
motor_default_ttl_ms = 750
[[motors]]
pwm_channel = 14
dir_pin_bcm = 23
reversed = true
"#
        )
        .unwrap();
        let config = Config::load(f.path()).unwrap();
        assert_eq!(config.motor_default_ttl_ms, 750);
        assert_eq!(config.motors.len(), 1);
        assert_eq!(config.motors[0].pwm_channel, 14);
        assert_eq!(config.motors[0].dir_pin_bcm, 23);
        assert!(config.motors[0].reversed);
    }

    #[test]
    fn malformed_toml_rejected() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "this is not valid toml {{{{").unwrap();
        let err = Config::load(f.path()).unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }));
    }

    #[test]
    fn env_overrides_apply() {
        let mut config = Config::default();
        config.apply_env_overrides(|key| match key {
            "NOMON_HAT_I2C_BUS" => Ok("3".into()),
            "NOMON_HAT_ADDRESS" => Ok("0x20".into()),
            "NOMON_HAT_LOG_LEVEL" => Ok("debug".into()),
            _ => Err(env::VarError::NotPresent),
        });

        assert_eq!(config.i2c_bus, 3);
        assert_eq!(config.hat_address, 0x20);
        assert_eq!(config.log_level, "debug");
    }

    #[test]
    fn defaults_include_servo_and_sensor_channels() {
        let config = Config::default();
        assert_eq!(config.servos.camera_pan, Some(0));
        assert_eq!(config.servos.camera_tilt, Some(1));
        assert_eq!(config.servos.steering, Some(2));
        assert_eq!(config.sensors.grayscale, [0, 1, 2]);
    }

    #[test]
    fn servo_channels_configurable_from_toml() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(
            f,
            r#"
[servos]
camera_pan = 3
camera_tilt = 4
steering = 5
"#
        )
        .unwrap();
        let config = Config::load(f.path()).unwrap();
        assert_eq!(config.servos.camera_pan, Some(3));
        assert_eq!(config.servos.camera_tilt, Some(4));
        assert_eq!(config.servos.steering, Some(5));
    }

    #[test]
    fn servo_channel_out_of_range_rejected() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "[servos]\nsteering = 12").unwrap();
        let err = Config::load(f.path()).unwrap_err();
        assert!(err.to_string().contains("servos"));
    }

    #[test]
    fn servo_channel_can_be_disabled() {
        let mut f = NamedTempFile::new().unwrap();
        // A partial [servos] table leaves unspecified fields as None.
        writeln!(f, "[servos]\ncamera_pan = 0\ncamera_tilt = 1").unwrap();
        let config = Config::load(f.path()).unwrap();
        // steering was not specified in the override, so it is None.
        assert_eq!(config.servos.steering, None);
        assert_eq!(config.servos.camera_pan, Some(0));
        assert_eq!(config.servos.camera_tilt, Some(1));
    }

    #[test]
    fn sensor_channels_configurable_from_toml() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "[sensors]\ngrayscale = [3, 4, 5]").unwrap();
        let config = Config::load(f.path()).unwrap();
        assert_eq!(config.sensors.grayscale, [3, 4, 5]);
    }

    #[test]
    fn grayscale_channel_out_of_range_rejected() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "[sensors]\ngrayscale = [0, 1, 8]").unwrap();
        let err = Config::load(f.path()).unwrap_err();
        assert!(err.to_string().contains("grayscale"));
    }

    #[test]
    fn duplicate_motor_pwm_channel_rejected() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(
            f,
            "[[motors]]\npwm_channel = 12\ndir_pin_bcm = 24\n\
             [[motors]]\npwm_channel = 12\ndir_pin_bcm = 23"
        )
        .unwrap();
        let err = Config::load(f.path()).unwrap_err();
        assert!(err.to_string().contains("pwm_channel"));
        assert!(err.to_string().contains("duplicated"));
    }

    #[test]
    fn duplicate_motor_dir_pin_rejected() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(
            f,
            "[[motors]]\npwm_channel = 12\ndir_pin_bcm = 24\n\
             [[motors]]\npwm_channel = 13\ndir_pin_bcm = 24"
        )
        .unwrap();
        let err = Config::load(f.path()).unwrap_err();
        assert!(err.to_string().contains("dir_pin_bcm"));
        assert!(err.to_string().contains("duplicated"));
    }

    /// Helper: write a complete `[routine]` TOML section with one field overridden.
    fn routine_toml(override_key: &str, override_val: &str) -> String {
        let defaults = [
            ("explore_speed_pct", "30.0"),
            ("obstacle_threshold_cm", "25.0"),
            ("cliff_threshold_normalized", "0.7"),
            ("loop_interval_ms", "100"),
            ("avoidance_backup_ms", "500"),
            ("avoidance_turn_angle_deg", "60.0"),
            ("max_duration_s", "300"),
        ];
        let mut lines = String::from("[routine]\n");
        for (k, v) in &defaults {
            let val = if *k == override_key { override_val } else { v };
            lines.push_str(&format!("{k} = {val}\n"));
        }
        lines
    }

    #[test]
    fn routine_config_rejects_speed_pct_zero() {
        let mut f = NamedTempFile::new().unwrap();
        write!(f, "{}", routine_toml("explore_speed_pct", "0.0")).unwrap();
        let err = Config::load(f.path()).unwrap_err();
        assert!(err.to_string().contains("explore_speed_pct"));
    }

    #[test]
    fn routine_config_rejects_speed_pct_over_100() {
        let mut f = NamedTempFile::new().unwrap();
        write!(f, "{}", routine_toml("explore_speed_pct", "101.0")).unwrap();
        let err = Config::load(f.path()).unwrap_err();
        assert!(err.to_string().contains("explore_speed_pct"));
    }

    #[test]
    fn routine_config_rejects_cliff_threshold_negative() {
        let mut f = NamedTempFile::new().unwrap();
        write!(f, "{}", routine_toml("cliff_threshold_normalized", "-0.1")).unwrap();
        let err = Config::load(f.path()).unwrap_err();
        assert!(err.to_string().contains("cliff_threshold_normalized"));
    }

    #[test]
    fn routine_config_rejects_loop_interval_ms_too_low() {
        let mut f = NamedTempFile::new().unwrap();
        write!(f, "{}", routine_toml("loop_interval_ms", "49")).unwrap();
        let err = Config::load(f.path()).unwrap_err();
        assert!(err.to_string().contains("loop_interval_ms"));
    }
}
