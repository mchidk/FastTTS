//! Minimal TOML configuration with environment overrides.
//!
//! English and Caps Lock hold-to-talk are fixed by design.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::{env, fs, path::PathBuf};

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Empty means the bundled ggml model under models/whisper.cpp.
    pub model: String,
    /// 0 = let whisper.cpp choose.
    pub threads: usize,
    pub initial_prompt: String,
    pub cleanup: bool,
    /// Maximum seconds of audio per streaming chunk.
    pub chunk_seconds: f32,
    /// Flush a phrase after this much silence.
    pub silence_ms: u64,
    /// Never flush a chunk shorter than this.
    pub min_chunk_seconds: f32,
    pub min_hold_ms: u64,
    pub max_record_seconds: u64,
    pub hot_mic: bool,
    pub sounds: bool,
    pub hud: bool,
    pub inject: String,
    pub char_delay_ms: u64,
    pub paste_delay_ms: u64,
    pub append_space: bool,
    pub mic_device: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            model: String::new(),
            threads: 0,
            initial_prompt: String::new(),
            cleanup: true,
            chunk_seconds: 4.0,
            silence_ms: 350,
            min_chunk_seconds: 0.8,
            min_hold_ms: 250,
            max_record_seconds: 120,
            hot_mic: true,
            sounds: false,
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
        let mut config = match config_path() {
            Some(path) if path.is_file() => {
                let text = fs::read_to_string(&path)
                    .with_context(|| format!("could not read {}", path.display()))?;
                toml::from_str::<Config>(&text)
                    .with_context(|| format!("invalid config {}", path.display()))?
            }
            _ => Self::default(),
        };
        config.apply_env();
        config.validate()?;
        Ok(config)
    }

    fn apply_env(&mut self) {
        // Explicit overrides avoid macro name tricks and keep types honest.
        if let Ok(v) = env::var("FASTTTS_MODEL") {
            self.model = v;
        }
        if let Ok(v) = env::var("FASTTTS_THREADS") {
            if let Ok(n) = v.parse() {
                self.threads = n;
            }
        }
        if let Ok(v) = env::var("FASTTTS_INITIAL_PROMPT") {
            self.initial_prompt = v;
        }
        if let Ok(v) = env::var("FASTTTS_CLEANUP") {
            self.cleanup = parse_bool(&v, self.cleanup);
        }
        if let Ok(v) = env::var("FASTTTS_CHUNK_SECONDS") {
            if let Ok(n) = v.parse() {
                self.chunk_seconds = n;
            }
        }
        if let Ok(v) = env::var("FASTTTS_SILENCE_MS") {
            if let Ok(n) = v.parse() {
                self.silence_ms = n;
            }
        }
        if let Ok(v) = env::var("FASTTTS_MIN_CHUNK_SECONDS") {
            if let Ok(n) = v.parse() {
                self.min_chunk_seconds = n;
            }
        }
        if let Ok(v) = env::var("FASTTTS_MIN_HOLD_MS") {
            if let Ok(n) = v.parse() {
                self.min_hold_ms = n;
            }
        }
        if let Ok(v) = env::var("FASTTTS_HOT_MIC") {
            self.hot_mic = parse_bool(&v, self.hot_mic);
        }
        if let Ok(v) = env::var("FASTTTS_SOUNDS") {
            self.sounds = parse_bool(&v, self.sounds);
        }
        if let Ok(v) = env::var("FASTTTS_HUD") {
            self.hud = parse_bool(&v, self.hud);
        }
        if let Ok(v) = env::var("FASTTTS_INJECT") {
            self.inject = v;
        }
        if let Ok(v) = env::var("FASTTTS_CHAR_DELAY_MS") {
            if let Ok(n) = v.parse() {
                self.char_delay_ms = n;
            }
        }
        if let Ok(v) = env::var("FASTTTS_PASTE_DELAY_MS") {
            if let Ok(n) = v.parse() {
                self.paste_delay_ms = n;
            }
        }
        if let Ok(v) = env::var("FASTTTS_MIC_DEVICE") {
            self.mic_device = v;
        }
    }

    fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            !self.initial_prompt.contains('\0'),
            "initial_prompt has a NUL byte"
        );
        anyhow::ensure!(self.min_hold_ms <= 5000, "min_hold_ms must be <= 5000");
        anyhow::ensure!(
            (0.5..=10.0).contains(&self.chunk_seconds),
            "chunk_seconds must be between 0.5 and 10"
        );
        anyhow::ensure!(self.silence_ms <= 3000, "silence_ms must be <= 3000");
        anyhow::ensure!(
            (0.1..=5.0).contains(&self.min_chunk_seconds),
            "min_chunk_seconds must be between 0.1 and 5"
        );
        anyhow::ensure!(
            self.max_record_seconds > 0,
            "max_record_seconds must be positive"
        );
        anyhow::ensure!(
            matches!(self.inject.as_str(), "clipboard" | "sendinput" | "none"),
            "inject must be clipboard, sendinput or none"
        );
        Ok(())
    }

    pub fn model_path(&self) -> PathBuf {
        if !self.model.is_empty() {
            return PathBuf::from(&self.model);
        }
        project_root().join("models/whisper.cpp/ggml-small.en.bin")
    }
}

fn parse_bool(value: &str, fallback: bool) -> bool {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => true,
        "0" | "false" | "no" | "off" => false,
        _ => fallback,
    }
}

pub fn config_path() -> Option<PathBuf> {
    if let Ok(path) = env::var("FASTTTS_CONFIG") {
        return Some(PathBuf::from(path));
    }
    env::var("APPDATA")
        .ok()
        .map(|root| PathBuf::from(root).join("FastTTS/config.toml"))
}

pub fn log_dir() -> PathBuf {
    if let Ok(root) = env::var("FASTTTS_DATA_DIR") {
        return PathBuf::from(root).join("logs");
    }
    let root = env::var("LOCALAPPDATA")
        .or_else(|_| env::var("APPDATA"))
        .unwrap_or_else(|_| "C:\\Users\\mich\\AppData\\Local".into());
    PathBuf::from(root).join("FastTTS/logs")
}

pub fn project_root() -> PathBuf {
    if let Ok(root) = env::var("FASTTTS_HOME") {
        return PathBuf::from(root);
    }
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    if cwd.join("models/whisper.cpp").is_dir() {
        return cwd;
    }
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("fasttts.exe"));
    let dir = exe.parent().unwrap_or(&exe).to_path_buf();
    // target/release/fasttts.exe -> project root; also accept exe beside models.
    for candidate in [dir.clone(), dir.join("../..").join("..")] {
        if candidate.join("models/whisper.cpp").is_dir() {
            return candidate;
        }
    }
    dir
}
