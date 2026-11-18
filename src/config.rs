//! Optional settings from `%APPDATA%\FastTTS\config.toml`.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::{env, fs, path::PathBuf};

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Remove fillers and stutters, fix capitals and spacing.
    pub cleanup: bool,
    /// Longest stretch of continuous speech sent to the model at once.
    pub chunk_seconds: f32,
    /// A pause this long ends a phrase, which is transcribed right away.
    pub silence_ms: u64,
    /// Shorter Caps Lock taps are ignored.
    pub min_hold_ms: u64,
    pub max_record_seconds: u64,
    pub hud: bool,
    /// "clipboard", "sendinput" or "none".
    pub inject: String,
    pub char_delay_ms: u64,
    pub paste_delay_ms: u64,
    pub append_space: bool,
    /// Empty means the Windows default microphone.
    pub mic_device: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            cleanup: true,
            chunk_seconds: 6.0,
            silence_ms: 500,
            min_hold_ms: 250,
            max_record_seconds: 300,
            hud: true,
            inject: "clipboard".into(),
            char_delay_ms: 10,
            paste_delay_ms: 400,
            append_space: true,
            mic_device: String::new(),
        }
    }
}

impl Config {
    pub fn load() -> Result<Self> {
        let Some(path) = config_path().filter(|path| path.is_file()) else {
            return Ok(Self::default());
        };
        let text = fs::read_to_string(&path)
            .with_context(|| format!("could not read {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("invalid settings in {}", path.display()))
    }

    fn parse(text: &str) -> Result<Self> {
        let config: Self = toml::from_str(text)?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            (2.0..=20.0).contains(&self.chunk_seconds),
            "chunk_seconds must be between 2 and 20"
        );
        anyhow::ensure!(
            (200..=3000).contains(&self.silence_ms),
            "silence_ms must be between 200 and 3000"
        );
        anyhow::ensure!(
            (self.silence_ms as f32) < self.chunk_seconds * 500.0,
            "silence_ms must be less than half of chunk_seconds"
        );
        anyhow::ensure!(self.min_hold_ms <= 1000, "min_hold_ms must be at most 1000");
        anyhow::ensure!(
            (5..=3600).contains(&self.max_record_seconds),
            "max_record_seconds must be between 5 and 3600"
        );
        anyhow::ensure!(
            matches!(self.inject.as_str(), "clipboard" | "sendinput" | "none"),
            "inject must be \"clipboard\", \"sendinput\" or \"none\""
        );
        anyhow::ensure!(
            self.char_delay_ms <= 200,
            "char_delay_ms must be at most 200"
        );
        anyhow::ensure!(
            (50..=5000).contains(&self.paste_delay_ms),
            "paste_delay_ms must be between 50 and 5000"
        );
        Ok(())
    }
}

pub fn config_path() -> Option<PathBuf> {
    env::var_os("APPDATA").map(|root| PathBuf::from(root).join("FastTTS").join("config.toml"))
}

pub fn log_dir() -> PathBuf {
    env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(env::temp_dir)
        .join("FastTTS")
        .join("logs")
}

#[cfg(test)]
mod tests {
    use super::Config;

    #[test]
    fn defaults_are_valid_and_every_key_parses() {
        Config::default().validate().unwrap();
        let config = Config::parse(
            r#"
            cleanup = false
            chunk_seconds = 8.0
            silence_ms = 400
            min_hold_ms = 300
            max_record_seconds = 60
            hud = false
            inject = "sendinput"
            char_delay_ms = 5
            paste_delay_ms = 300
            append_space = false
            mic_device = "Headset"
            "#,
        )
        .unwrap();
        assert!(!config.hud);
        assert_eq!(config.inject, "sendinput");
    }

    #[test]
    fn rejects_typos_and_bad_values() {
        assert!(Config::parse("chunk_secs = 4.0").is_err());
        assert!(Config::parse("inject = \"keyboard\"").is_err());
        assert!(Config::parse("silence_ms = 50").is_err());
        assert!(Config::parse(
            "chunk_seconds = 2.0
silence_ms = 1500"
        )
        .is_err());
    }
}
