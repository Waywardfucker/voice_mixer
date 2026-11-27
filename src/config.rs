use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone, PartialEq)]
#[serde(default)]
pub struct AppCfg {
    pub label: String,
    pub exe: String,
    pub enabled: bool,
    pub volume: f32,
    pub delay_ms: f32,
}

impl Default for AppCfg {
    fn default() -> Self {
        Self { label: String::new(), exe: String::new(), enabled: true, volume: 0.8, delay_ms: 0.0 }
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq)]
#[serde(default)]
pub struct Config {
    /// Часть имени устройства вывода (виртуальный кабель)
    pub output: String,
    /// Микрофоны по приоритету: части имён устройств
    pub mics: Vec<String>,
    pub mic_volume: f32,
    pub mic_delay_ms: f32,
    pub apps: Vec<AppCfg>,
    pub running: bool,
    pub tray_on_close: bool,
    /// Прослушивание итогового микса в отдельном устройстве (наушники)
    pub monitor_on: bool,
    pub monitor_device: String,
    pub monitor_volume: f32,
    /// Шумоподавление микрофона (RNNoise)
    pub denoise_on: bool,
    pub denoise_amount: f32,
    /// Гейт по вероятности голоса (VAD из RNNoise)
    pub gate_on: bool,
    pub gate_threshold: f32,
    pub gate_hold_ms: f32,
    pub gate_depth_db: f32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            output: "Line 2".into(),
            mics: vec!["fifine SC8".into(), "MCHOSE".into(), "TONOR".into()],
            mic_volume: 1.0,
            mic_delay_ms: 0.0,
            apps: vec![
                AppCfg { label: "Яндекс Браузер".into(), exe: "browser.exe".into(), ..Default::default() },
                AppCfg { label: "Яндекс Музыка".into(), exe: "Яндекс Музыка.exe".into(), ..Default::default() },
            ],
            running: true,
            tray_on_close: true,
            monitor_on: false,
            monitor_device: String::new(),
            monitor_volume: 0.7,
            denoise_on: false,
            denoise_amount: 1.0,
            gate_on: false,
            gate_threshold: 0.5,
            gate_hold_ms: 200.0,
            gate_depth_db: 40.0,
        }
    }
}

impl Config {
    pub fn path() -> PathBuf {
        let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
        base.join("VoiceMixer").join("config.toml")
    }

    pub fn load() -> Self {
        std::fs::read_to_string(Self::path())
            .ok()
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let p = Self::path();
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        if let Ok(s) = toml::to_string_pretty(self) {
            let _ = std::fs::write(p, s);
        }
    }
}
