// ALSA mixer control for output volume (speaker) and input gain
// (USB microphone PCM2902).
//
// Cards are resolved by name against /proc/asound/cards at call time —
// ALSA card numbers depend on driver load order, so a hardcoded index goes
// stale (the old default of card 2 pointed at a card that does not exist on
// the robot, so every mic-gain call failed).  An explicit configured index
// still wins as an operator override.
//
// Production implementation uses the ALSA `amixer` command-line utility.
// Tests substitute a `MockAlsaControl` that records calls without hitting
// the system mixer.

use std::process::Command;

use thiserror::Error;

/// Errors from ALSA mixer operations.
#[derive(Debug, Error)]
pub enum AudioError {
    #[error("amixer returned an error: {0}")]
    Command(String),
    #[error("failed to parse amixer output: {0}")]
    Parse(String),
    #[error("I/O error invoking amixer: {0}")]
    Io(#[from] std::io::Error),
}

/// Abstraction over ALSA mixer control.
///
/// Sync (blocking) so it can be used from `tokio::task::spawn_blocking`.
/// Test code supplies a `MockAlsaControl`; production code uses
/// `AmixerControl`.
pub trait AlsaControl: Send + Sync {
    /// Return the current output volume as a percentage (0–100).
    fn get_volume_pct(&self) -> Result<u8, AudioError>;
    /// Set the output volume to `pct` percent (0–100).
    fn set_volume_pct(&self, pct: u8) -> Result<(), AudioError>;
    /// Return the current microphone capture gain as a percentage (0–100).
    fn get_mic_gain_pct(&self) -> Result<u8, AudioError>;
    /// Set the microphone capture gain to `pct` percent (0–100).
    fn set_mic_gain_pct(&self, pct: u8) -> Result<(), AudioError>;
}

/// Production `AlsaControl` backed by `amixer`.
///
/// `amixer -c <card_index> sset "<control>" <pct>%` — works on any ALSA
/// system with the `alsa-utils` package installed (standard on Raspberry Pi OS).
#[derive(Debug)]
pub struct AmixerControl {
    /// Explicit ALSA card index for audio output; None = resolve by name.
    pub(crate) output_card_index: Option<u8>,
    /// Name fragment matched against `/proc/asound/cards` for the output
    /// card, e.g. `"sndrpihifiberry"`.
    pub(crate) output_card_name: String,
    /// ALSA mixer control name for output volume, e.g. `"Master"`.
    pub(crate) output_control: String,
    /// Explicit ALSA card index for audio input; None = resolve by name.
    pub(crate) input_card_index: Option<u8>,
    /// Name fragment matched against `/proc/asound/cards` for the input
    /// card, e.g. `"USB"` (the PCM2902 mic codec).
    pub(crate) input_card_name: String,
    /// ALSA mixer capture controls driven for microphone gain, in priority
    /// order, e.g. `["Mic", "Capture"]`. Each control that exists on the card
    /// is set (absent ones skipped); the first present one is read back. USB
    /// codecs vary in whether the ADC level is named `"Mic"` or `"Capture"`.
    pub(crate) input_controls: Vec<String>,
}

impl AmixerControl {
    /// Resolve an ALSA card index: explicit override, else case-insensitive
    /// name match against `/proc/asound/cards`, else None (amixer's default
    /// card).
    fn resolve_card_index(explicit: Option<u8>, name_match: &str) -> Option<u8> {
        if explicit.is_some() {
            return explicit;
        }
        match std::fs::read_to_string("/proc/asound/cards") {
            Ok(cards) => Self::find_card(&cards, name_match),
            Err(_) => None,
        }
    }

    /// Find the first card in `/proc/asound/cards` content whose entry
    /// contains `name_match` (case-insensitive).
    ///
    /// Card header lines look like
    /// ` 1 [Device         ]: USB-Audio - USB PnP Sound Device`; the
    /// following continuation line carries the long device description, so
    /// both are matched against while tracking the current card index.
    fn find_card(cards: &str, name_match: &str) -> Option<u8> {
        let needle = name_match.trim().to_lowercase();
        if needle.is_empty() {
            return None;
        }
        let mut current: Option<u8> = None;
        for line in cards.lines() {
            let trimmed = line.trim_start();
            if let Some((idx_str, rest)) = trimmed.split_once(' ')
                && rest.trim_start().starts_with('[')
                && let Ok(idx) = idx_str.parse::<u8>()
            {
                current = Some(idx);
            }
            if current.is_some() && line.to_lowercase().contains(&needle) {
                return current;
            }
        }
        None
    }
    /// The simple-mixer control names available on `card_index`.
    ///
    /// Runs `amixer -c N scontrols` and parses the quoted control names.  Used
    /// to skip absent capture controls before setting gain, and to enrich
    /// error messages so the caller sees what names are valid on their hardware.
    /// Returns an empty vec if amixer cannot be run.
    fn available_control_names(card_index: Option<u8>) -> Vec<String> {
        let mut cmd = Command::new("amixer");
        if let Some(idx) = card_index {
            cmd.args(["-c", &idx.to_string(), "scontrols"]);
        } else {
            cmd.args(["scontrols"]);
        }
        match cmd.output() {
            Ok(out) => Self::parse_control_names(&String::from_utf8_lossy(&out.stdout)),
            Err(_) => Vec::new(),
        }
    }

    /// Parse control names from `amixer scontrols` output.
    ///
    /// Lines look like `Simple mixer control 'Mic',0`; the quoted name is
    /// returned (`["Master", "Capture", "Mic"]`).
    fn parse_control_names(text: &str) -> Vec<String> {
        text.lines()
            .filter_map(|line| {
                let start = line.find('\'')?;
                let rest = &line[start + 1..];
                let end = rest.find('\'')?;
                Some(rest[..end].to_string())
            })
            .collect()
    }

    /// Configured controls that exist on the card (case-insensitive),
    /// preserving configured priority order and using the card's own casing.
    fn select_controls(configured: &[String], available: &[String]) -> Vec<String> {
        configured
            .iter()
            .filter_map(|want| {
                available
                    .iter()
                    .find(|have| have.eq_ignore_ascii_case(want))
                    .cloned()
            })
            .collect()
    }

    /// Human-readable list of the card's controls, for error messages.
    fn available_controls(card_index: Option<u8>) -> String {
        let names = Self::available_control_names(card_index);
        if names.is_empty() {
            match card_index {
                Some(i) => format!("(no controls found on card {i})"),
                None => "(no controls found on default card)".to_string(),
            }
        } else {
            names.join(", ")
        }
    }

    /// Error describing that none of the configured mic controls exist on the
    /// card, listing what the card actually offers.
    fn no_mic_control_error(&self, card: Option<u8>, available: &[String]) -> AudioError {
        AudioError::Command(format!(
            "none of the configured mic controls {:?} exist on card {}; available: {}",
            self.input_controls,
            card.map(|i| i.to_string())
                .unwrap_or_else(|| "(default)".to_string()),
            if available.is_empty() {
                "(none)".to_string()
            } else {
                available.join(", ")
            },
        ))
    }

    /// Run `amixer` with the given arguments (prefixed with `-c <card_index>`
    /// when set) and return stdout.
    ///
    /// On a non-zero exit status the error message is enriched with the list
    /// of available mixer controls on the card so misconfigured control names
    /// are immediately diagnosable.
    fn run_amixer(card_index: Option<u8>, args: &[&str]) -> Result<String, AudioError> {
        let mut cmd = Command::new("amixer");
        if let Some(idx) = card_index {
            cmd.args(["-c", &idx.to_string()]);
        }
        cmd.args(args);
        let out = cmd.output()?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
            let available = Self::available_controls(card_index);
            return Err(AudioError::Command(format!(
                "{stderr}Available controls on card {}: {available}",
                card_index
                    .map(|i| i.to_string())
                    .unwrap_or_else(|| "(default)".to_string())
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Parse the current percentage from `amixer get` output.
    ///
    /// Looks for the first `[N%]` token in the output — produced by lines
    /// like `  Front Left: Playback 84 [84%] ...`.
    fn parse_pct(output: &str) -> Result<u8, AudioError> {
        for line in output.lines() {
            if let Some(start) = line.find('[')
                && let Some(end) = line[start..].find('%')
            {
                let pct_str = line[start + 1..start + end].trim();
                return pct_str.parse::<u8>().map_err(|_| {
                    AudioError::Parse(format!(
                        "could not parse percentage '{pct_str}' from amixer output"
                    ))
                });
            }
        }
        Err(AudioError::Parse(format!(
            "no percentage found in amixer output: {output}"
        )))
    }
}

impl AmixerControl {
    /// The output card for this call (explicit index or name-resolved).
    fn output_card(&self) -> Option<u8> {
        Self::resolve_card_index(self.output_card_index, &self.output_card_name)
    }

    /// The input card for this call (explicit index or name-resolved).
    fn input_card(&self) -> Option<u8> {
        Self::resolve_card_index(self.input_card_index, &self.input_card_name)
    }
}

impl AlsaControl for AmixerControl {
    fn get_volume_pct(&self) -> Result<u8, AudioError> {
        let out = Self::run_amixer(self.output_card(), &["get", &self.output_control])?;
        Self::parse_pct(&out)
    }

    fn set_volume_pct(&self, pct: u8) -> Result<(), AudioError> {
        let pct_arg = format!("{pct}%");
        Self::run_amixer(
            self.output_card(),
            &["sset", &self.output_control, &pct_arg],
        )?;
        Ok(())
    }

    fn get_mic_gain_pct(&self) -> Result<u8, AudioError> {
        let card = self.input_card();
        let available = Self::available_control_names(card);
        let targets = Self::select_controls(&self.input_controls, &available);
        if targets.is_empty() {
            return Err(self.no_mic_control_error(card, &available));
        }
        // Read the first present control that yields a percentage (some
        // controls, e.g. a capture *switch*, carry no level).
        let mut last_err: Option<AudioError> = None;
        for control in &targets {
            match Self::run_amixer(card, &["get", control]).and_then(|o| Self::parse_pct(&o)) {
                Ok(pct) => return Ok(pct),
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.expect("targets non-empty implies at least one attempt"))
    }

    fn set_mic_gain_pct(&self, pct: u8) -> Result<(), AudioError> {
        let card = self.input_card();
        let available = Self::available_control_names(card);
        let targets = Self::select_controls(&self.input_controls, &available);
        if targets.is_empty() {
            return Err(self.no_mic_control_error(card, &available));
        }
        // Set every present control; succeed if at least one takes (a present
        // control can still reject sset, e.g. a read-only capture switch).
        let pct_arg = format!("{pct}%");
        let mut set_any = false;
        let mut last_err: Option<AudioError> = None;
        for control in &targets {
            match Self::run_amixer(card, &["sset", control, &pct_arg]) {
                Ok(_) => set_any = true,
                Err(e) => last_err = Some(e),
            }
        }
        if set_any {
            Ok(())
        } else {
            Err(last_err.expect("targets non-empty implies at least one attempt"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verbatim `/proc/asound/cards` content from the perceptua robot.
    const ROBOT_CARDS: &str = "\
 0 [vc4hdmi        ]: vc4-hdmi - vc4-hdmi
                      vc4-hdmi
 1 [Device         ]: USB-Audio - USB PnP Sound Device
                      C-Media Electronics Inc. USB PnP Sound Device at usb-3f980000.usb-1, full speed
";

    #[test]
    fn find_card_matches_usb_mic_by_name() {
        assert_eq!(AmixerControl::find_card(ROBOT_CARDS, "USB"), Some(1));
    }

    #[test]
    fn find_card_is_case_insensitive() {
        assert_eq!(AmixerControl::find_card(ROBOT_CARDS, "usb"), Some(1));
    }

    #[test]
    fn find_card_matches_bracketed_card_id() {
        assert_eq!(AmixerControl::find_card(ROBOT_CARDS, "vc4hdmi"), Some(0));
    }

    #[test]
    fn find_card_matches_continuation_description_line() {
        assert_eq!(AmixerControl::find_card(ROBOT_CARDS, "c-media"), Some(1));
    }

    #[test]
    fn find_card_none_when_no_match() {
        assert_eq!(
            AmixerControl::find_card(ROBOT_CARDS, "sndrpihifiberry"),
            None
        );
    }

    #[test]
    fn find_card_none_for_empty_needle() {
        assert_eq!(AmixerControl::find_card(ROBOT_CARDS, ""), None);
        assert_eq!(AmixerControl::find_card(ROBOT_CARDS, "   "), None);
    }

    #[test]
    fn find_card_matches_hifiberry_when_overlay_enabled() {
        // Layout once `dtoverlay=hifiberry-dac` is enabled on the Pi.
        let cards = "\
 0 [vc4hdmi        ]: vc4-hdmi - vc4-hdmi
                      vc4-hdmi
 1 [sndrpihifiberry]: HifiBerry-DAC - snd_rpi_hifiberry_dac
                      snd_rpi_hifiberry_dac
 2 [Device         ]: USB-Audio - USB PnP Sound Device
                      C-Media Electronics Inc. USB PnP Sound Device at usb-3f980000.usb-1, full speed
";
        assert_eq!(AmixerControl::find_card(cards, "sndrpihifiberry"), Some(1));
        assert_eq!(AmixerControl::find_card(cards, "USB"), Some(2));
    }

    #[test]
    fn resolve_card_index_explicit_override_wins() {
        assert_eq!(AmixerControl::resolve_card_index(Some(7), "usb"), Some(7));
    }

    #[test]
    fn parse_pct_extracts_first_percentage() {
        let output = "Simple mixer control 'Digital',0\n  \
            Limits: Playback 0 - 255\n  \
            Mono: Playback 214 [84%] [-12.28dB]\n";
        let pct = AmixerControl::parse_pct(output).unwrap();
        assert_eq!(pct, 84);
    }

    #[test]
    fn parse_pct_handles_zero() {
        let output = "  Front Left: Playback 0 [0%] [-inf]\n";
        let pct = AmixerControl::parse_pct(output).unwrap();
        assert_eq!(pct, 0);
    }

    #[test]
    fn parse_pct_handles_100() {
        let output = "  Mono: Playback 255 [100%] [0.00dB]\n";
        let pct = AmixerControl::parse_pct(output).unwrap();
        assert_eq!(pct, 100);
    }

    #[test]
    fn parse_pct_error_on_no_percentage() {
        let output = "No controls found.\n";
        let err = AmixerControl::parse_pct(output).unwrap_err();
        assert!(matches!(err, AudioError::Parse(_)));
    }

    /// Verbatim `amixer -c 1 scontrols` shape for a USB PCM2902 codec.
    const USB_SCONTROLS: &str = "\
Simple mixer control 'Speaker',0
Simple mixer control 'Mic',0
Simple mixer control 'Auto Gain Control',0
";

    #[test]
    fn parse_control_names_extracts_quoted_names() {
        assert_eq!(
            AmixerControl::parse_control_names(USB_SCONTROLS),
            vec!["Speaker", "Mic", "Auto Gain Control"]
        );
    }

    #[test]
    fn parse_control_names_empty_for_blank_output() {
        assert!(AmixerControl::parse_control_names("").is_empty());
        assert!(AmixerControl::parse_control_names("no controls found\n").is_empty());
    }

    #[test]
    fn select_controls_keeps_present_in_configured_order() {
        let configured = vec!["Mic".to_string(), "Capture".to_string()];
        let available = vec![
            "Capture".to_string(),
            "Mic".to_string(),
            "Speaker".to_string(),
        ];
        // Configured order wins (Mic before Capture), not the card's order.
        assert_eq!(
            AmixerControl::select_controls(&configured, &available),
            vec!["Mic", "Capture"]
        );
    }

    #[test]
    fn select_controls_skips_absent_and_is_case_insensitive() {
        let configured = vec!["mic".to_string(), "Capture".to_string()];
        let available = vec!["Mic".to_string(), "Speaker".to_string()];
        // "Capture" is absent (skipped); "mic" matches "Mic" and returns the
        // card's actual casing.
        assert_eq!(
            AmixerControl::select_controls(&configured, &available),
            vec!["Mic"]
        );
    }

    #[test]
    fn select_controls_empty_when_none_present() {
        let configured = vec!["Mic".to_string(), "Capture".to_string()];
        let available = vec!["Speaker".to_string()];
        assert!(AmixerControl::select_controls(&configured, &available).is_empty());
    }
}
