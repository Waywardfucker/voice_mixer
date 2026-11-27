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

/// Звук для Soundpad.
#[derive(Serialize, Deserialize, Clone, PartialEq)]
#[serde(default)]
pub struct SoundCfg {
    pub name: String,
    pub path: String,
    pub volume: f32,
    /// Зациклить: повторное нажатие бинда останавливает звук.
    pub looped: bool,
    /// Глобальный бинд, например «Ctrl+Alt+1». Пусто — без бинда.
    pub key: String,
}

impl Default for SoundCfg {
    fn default() -> Self {
        Self { name: String::new(), path: String::new(), volume: 1.0, looped: false, key: String::new() }
    }
}

/// Глобальный бинд на действие (выключить микрофон, переключить шумоподавление и т. д.).
#[derive(Serialize, Deserialize, Clone, PartialEq)]
#[serde(default)]
pub struct BindCfg {
    pub action: String,
    pub key: String,
}

impl Default for BindCfg {
    fn default() -> Self {
        Self { action: "toggle_mic_mute".into(), key: String::new() }
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq)]
#[serde(default)]
pub struct Config {
    /// Часть имени устройства вывода (виртуальный кабель)
    pub output: String,
    /// Микрофоны по приоритету: части имён устройств.
    /// Если список пуст, используется микрофон Windows по умолчанию.
    pub mics: Vec<String>,
    pub mic_volume: f32,
    pub mic_delay_ms: f32,
    pub mic_muted: bool,
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
    /// Название темы оформления
    pub theme: String,
    /// Общая громкость Soundpad
    pub pad_volume: f32,
    pub sounds: Vec<SoundCfg>,
    pub binds: Vec<BindCfg>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            output: "Line 2".into(),
            mics: vec![],
            mic_volume: 1.0,
            mic_delay_ms: 0.0,
            mic_muted: false,
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
            theme: "Фиолет".into(),
            pad_volume: 1.0,
            sounds: vec![],
            binds: vec![],
        }
    }
}

impl Config {
    fn base_dir() -> PathBuf {
        let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
        base.join("VoiceMixer")
    }

    pub fn path() -> PathBuf {
        Self::base_dir().join("config.toml")
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

    // ───────── экспорт / импорт ─────────

    pub fn to_toml_string(&self) -> Result<String, String> {
        toml::to_string_pretty(self).map_err(|e| e.to_string())
    }

    pub fn from_toml_str(s: &str) -> Result<Self, String> {
        toml::from_str(s).map_err(|e| e.to_string())
    }

    // ───────── профили ─────────

    fn profiles_dir() -> PathBuf {
        Self::base_dir().join("profiles")
    }

    fn profile_path(name: &str) -> Option<PathBuf> {
        let clean: String = name
            .trim()
            .chars()
            .filter(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.'))
            .collect();
        let clean = clean.trim().trim_matches('.').to_string();
        if clean.is_empty() {
            None
        } else {
            Some(Self::profiles_dir().join(format!("{clean}.toml")))
        }
    }

    pub fn list_profiles() -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(Self::profiles_dir())
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .filter_map(|e| {
                        let p = e.path();
                        if p.extension().and_then(|x| x.to_str()) == Some("toml") {
                            p.file_stem().and_then(|s| s.to_str()).map(|s| s.to_string())
                        } else {
                            None
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        v.sort_by_key(|s| s.to_lowercase());
        v
    }

    pub fn save_profile(&self, name: &str) -> Result<(), String> {
        let p = Self::profile_path(name).ok_or("введите имя профиля")?;
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
        }
        std::fs::write(p, self.to_toml_string()?).map_err(|e| e.to_string())
    }

    pub fn load_profile(name: &str) -> Result<Self, String> {
        let p = Self::profile_path(name).ok_or("неверное имя профиля")?;
        let s = std::fs::read_to_string(p).map_err(|e| e.to_string())?;
        Self::from_toml_str(&s)
    }

    pub fn delete_profile(name: &str) {
        if let Some(p) = Self::profile_path(name) {
            let _ = std::fs::remove_file(p);
        }
    }
}
