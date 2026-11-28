#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod engine;
mod soundpad;

use config::{AppCfg, BindCfg, Config, SoundCfg};
use global_hotkey::hotkey::HotKey;
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, RwLock};
use eframe::egui::{self, Align, Color32, Layout, RichText, Rounding, Sense, Stroke, TextEdit, vec2};
use engine::Shared;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use winreg::{enums::HKEY_CURRENT_USER, RegKey};

#[derive(Clone, Copy)]
struct Palette {
    bg: Color32,
    card: Color32,
    border: Color32,
    accent: Color32,
    accent_light: Color32,
    dim: Color32,
}

const fn pal_of(bg: [u8; 3], card: [u8; 3], border: [u8; 3], accent: [u8; 3], light: [u8; 3], dim: [u8; 3]) -> Palette {
    Palette {
        bg: Color32::from_rgb(bg[0], bg[1], bg[2]),
        card: Color32::from_rgb(card[0], card[1], card[2]),
        border: Color32::from_rgb(border[0], border[1], border[2]),
        accent: Color32::from_rgb(accent[0], accent[1], accent[2]),
        accent_light: Color32::from_rgb(light[0], light[1], light[2]),
        dim: Color32::from_rgb(dim[0], dim[1], dim[2]),
    }
}

const THEMES: [(&str, Palette); 6] = [
    ("Фиолет", pal_of([17, 17, 23], [26, 26, 36], [44, 44, 60], [124, 92, 255], [170, 150, 255], [140, 140, 160])),
    ("Океан", pal_of([12, 18, 26], [19, 28, 40], [36, 52, 72], [40, 150, 230], [120, 200, 255], [130, 150, 170])),
    ("Лес", pal_of([14, 20, 17], [21, 31, 26], [38, 58, 48], [52, 180, 110], [130, 225, 170], [135, 160, 145])),
    ("Закат", pal_of([22, 15, 17], [33, 22, 25], [62, 40, 46], [240, 110, 70], [255, 170, 130], [170, 145, 145])),
    ("Неон", pal_of([14, 12, 20], [24, 18, 34], [58, 36, 80], [230, 60, 200], [255, 140, 235], [160, 140, 175])),
    ("Графит", pal_of([18, 18, 18], [28, 28, 28], [52, 52, 52], [110, 122, 145], [190, 200, 220], [150, 150, 150])),
];

static PAL: RwLock<Palette> = RwLock::new(THEMES[0].1);

fn pal() -> Palette {
    *PAL.read().unwrap()
}

fn set_theme(name: &str) {
    let p = THEMES.iter().find(|(n, _)| *n == name).map(|(_, p)| *p).unwrap_or(THEMES[0].1);
    *PAL.write().unwrap() = p;
}

const GREEN: Color32 = Color32::from_rgb(70, 200, 120);
const YELLOW: Color32 = Color32::from_rgb(230, 190, 60);
const RED: Color32 = Color32::from_rgb(230, 80, 90);

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const PRESETS: [(&str, &str); 6] = [
    ("Яндекс Браузер", "browser.exe"),
    ("Яндекс Музыка", "Яндекс Музыка.exe"),
    ("Chrome", "chrome.exe"),
    ("Edge", "msedge.exe"),
    ("Firefox", "firefox.exe"),
    ("Spotify", "Spotify.exe"),
];

fn main() -> eframe::Result<()> {
    let minimized = std::env::args().any(|a| a == "--minimized");
    let cfg = Config::load();
    cfg.save();
    set_theme(&cfg.theme);

    let shared = Shared::new();
    let tx = engine::spawn(shared.clone(), cfg.clone());

    let hidden = minimized && cfg.tray_on_close;
    VISIBLE.store(!hidden, Ordering::Relaxed);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Voice Mixer")
            .with_inner_size([660.0, 800.0])
            .with_min_inner_size([540.0, 480.0])
            .with_icon(egui::IconData { rgba: icon_rgba(128), width: 128, height: 128 })
            .with_visible(!hidden),
        ..Default::default()
    };

    eframe::run_native(
        "Voice Mixer",
        options,
        Box::new(move |cc| {
            apply_style(&cc.egui_ctx);
            Ok(Box::new(App::new(&cc.egui_ctx, cfg, tx, shared)))
        }),
    )
}

// ───────────────────────────── Приложение ─────────────────────────────

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Mixer,
    Pad,
    Binds,
    Settings,
}

/// Что сейчас назначается: ждём нажатие сочетания клавиш.
#[derive(Clone, Copy, PartialEq)]
enum Target {
    Sound(usize),
    Bind(usize),
}

#[derive(Clone)]
enum Act {
    Sound(usize),
    Action(String),
}

/// Состояние, доступное потоку горячих клавиш (работает и когда окно скрыто в трее).
struct HotRt {
    cfg: Arc<Mutex<Config>>,
    tx: Mutex<Sender<Config>>,
    shared: Arc<Shared>,
    dirty: Arc<AtomicBool>,
}

impl HotRt {
    fn run(&self, act: Act, ctx: &egui::Context) {
        match act {
            Act::Sound(i) => {
                let c = self.cfg.lock().unwrap();
                play_cfg_sound(&self.shared, &c, i);
            }
            Act::Action(name) => {
                if name == "stop_sounds" {
                    self.shared.pad.stop_all();
                    return;
                }
                if name == "show_window" {
                    show_window(ctx);
                    return;
                }
                let snapshot = {
                    let mut c = self.cfg.lock().unwrap();
                    match name.as_str() {
                        "toggle_mic_mute" => c.mic_muted = !c.mic_muted,
                        "toggle_denoise" => c.denoise_on = !c.denoise_on,
                        "toggle_gate" => c.gate_on = !c.gate_on,
                        "toggle_running" => c.running = !c.running,
                        "toggle_monitor" => c.monitor_on = !c.monitor_on,
                        n if n.starts_with("app:") => {
                            let exe = n[4..].to_string();
                            match c.apps.iter_mut().find(|a| a.exe.eq_ignore_ascii_case(&exe)) {
                                Some(a) => a.enabled = !a.enabled,
                                None => return,
                            }
                        }
                        _ => return,
                    }
                    c.clone()
                };
                snapshot.save();
                let _ = self.tx.lock().unwrap().send(snapshot);
                self.dirty.store(true, Ordering::Relaxed);
                ctx.request_repaint();
            }
        }
    }
}

fn start_hotkey_thread(ctx: egui::Context, map: Arc<Mutex<HashMap<u32, Act>>>, rt: Arc<HotRt>) {
    std::thread::Builder::new()
        .name("hotkeys".into())
        .spawn(move || {
            let rx = GlobalHotKeyEvent::receiver();
            while let Ok(ev) = rx.recv() {
                if ev.state != HotKeyState::Pressed {
                    continue;
                }
                let act = map.lock().unwrap().get(&ev.id).cloned();
                if let Some(a) = act {
                    rt.run(a, &ctx);
                }
            }
        })
        .ok();
}

/// Действия, на которые можно повесить бинд: (id, название).
fn bind_actions(cfg: &Config) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = vec![
        ("toggle_mic_mute".into(), "Выключить / включить микрофон".into()),
        ("toggle_denoise".into(), "Шумоподавление вкл / выкл".into()),
        ("toggle_gate".into(), "Гейт по голосу вкл / выкл".into()),
        ("toggle_monitor".into(), "Прослушивание вкл / выкл".into()),
        ("toggle_running".into(), "Запустить / остановить микшер".into()),
        ("stop_sounds".into(), "Остановить все звуки Soundpad".into()),
        ("show_window".into(), "Показать окно".into()),
    ];
    for a in &cfg.apps {
        let name = if a.label.is_empty() { a.exe.clone() } else { a.label.clone() };
        v.push((format!("app:{}", a.exe), format!("Звук приложения вкл / выкл: {name}")));
    }
    v
}

fn action_label(cfg: &Config, id: &str) -> String {
    bind_actions(cfg)
        .into_iter()
        .find(|(i, _)| i == id)
        .map(|(_, l)| l)
        .unwrap_or_else(|| id.to_string())
}

fn signature(cfg: &Config) -> String {
    let mut s = String::new();
    for (i, x) in cfg.sounds.iter().enumerate() {
        if !x.key.is_empty() {
            s.push_str(&format!("S{i}={};", x.key));
        }
    }
    for x in &cfg.binds {
        if !x.key.is_empty() {
            s.push_str(&format!("B{}={};", x.action, x.key));
        }
    }
    s
}

/// Имя клавиши в формате, понятном global-hotkey. None — клавиша не поддерживается.
fn key_name(k: egui::Key) -> Option<String> {
    let d = format!("{:?}", k);
    match d.as_str() {
        "Space" | "Enter" | "Tab" | "Backspace" | "Insert" | "Delete" | "Home" | "End" | "PageUp" | "PageDown"
        | "ArrowUp" | "ArrowDown" | "ArrowLeft" | "ArrowRight" => Some(d.clone()),
        _ if d.len() == 1 && d.chars().all(|c| c.is_ascii_uppercase()) => Some(d.clone()),
        _ if d.starts_with("Num") && d.len() == 4 => Some(d[3..].to_string()),
        _ if d.len() > 1 && d.starts_with('F') && d[1..].chars().all(|c| c.is_ascii_digit()) => Some(d.clone()),
        _ => None,
    }
}

fn is_fkey(name: &str) -> bool {
    name.len() > 1 && name.starts_with('F') && name[1..].chars().all(|c| c.is_ascii_digit())
}

/// Значение фильтра «Без группы» (служебная строка, не совпадает с названием группы).
const NO_GROUP: &str = "\u{1}no-group";

/// Выбор режима воспроизведения звука.
fn mode_combo(ui: &mut egui::Ui, id: impl std::hash::Hash, v: &mut Option<bool>) {
    let label = match *v {
        None => "как в настройках",
        Some(true) => "останавливает другие",
        Some(false) => "играет поверх",
    };
    egui::ComboBox::from_id_salt(id).selected_text(label).show_ui(ui, |ui| {
        ui.selectable_value(v, None, "как в настройках");
        ui.selectable_value(v, Some(true), "останавливает другие");
        ui.selectable_value(v, Some(false), "играет поверх");
    });
}

/// Проиграть звук `i` из конфига с учётом режима (общего или своего).
fn play_cfg_sound(shared: &Shared, cfg: &Config, i: usize) {
    if let Some(s) = cfg.sounds.get(i) {
        let (exclusive, layer) = match s.exclusive {
            Some(true) => (true, false),
            Some(false) => (false, true),
            None => (cfg.pad_exclusive, false),
        };
        shared.pad.play(&s.path, s.volume, s.looped, exclusive, layer);
    }
}

fn add_sound(cfg: &mut Config, path: &str) -> bool {
    let p = std::path::Path::new(path);
    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    if !soundpad::EXTENSIONS.contains(&ext.as_str()) || cfg.sounds.iter().any(|s| s.path == path) {
        return false;
    }
    let name = p.file_stem().and_then(|n| n.to_str()).unwrap_or("звук").to_string();
    let volume = cfg.pad_default_volume;
    cfg.sounds.push(SoundCfg { name, path: path.to_string(), volume, ..Default::default() });
    true
}

struct App {
    cfg: Config,
    tx: Sender<Config>,
    shared: Arc<Shared>,
    new_app: String,
    autostart: bool,
    tray: Option<TrayIcon>,
    tab: Tab,
    // горячие клавиши
    shared_cfg: Arc<Mutex<Config>>,
    dirty: Arc<AtomicBool>,
    hk: Option<GlobalHotKeyManager>,
    hk_map: Arc<Mutex<HashMap<u32, Act>>>,
    hk_regs: Vec<HotKey>,
    hk_sig: String,
    hk_msgs: Vec<String>,
    capture: Option<Target>,
    capture_msg: String,
    preloaded_rate: u32,
    last_trim: Instant,
    // Soundpad: поиск, фильтры, страницы
    pad_search: String,
    pad_group: String,
    pad_page: usize,
    pad_sort: bool,
    pad_filter_sig: String,
    pad_remove: Option<usize>,
    // профили и конфиги
    profile_name: String,
    profiles: Vec<String>,
    io_msg: String,
}

impl App {
    fn new(ctx: &egui::Context, cfg: Config, tx: Sender<Config>, shared: Arc<Shared>) -> Self {
        let shared_cfg = Arc::new(Mutex::new(cfg.clone()));
        let dirty = Arc::new(AtomicBool::new(false));
        let hk_map: Arc<Mutex<HashMap<u32, Act>>> = Arc::new(Mutex::new(HashMap::new()));
        let rt = Arc::new(HotRt {
            cfg: shared_cfg.clone(),
            tx: Mutex::new(tx.clone()),
            shared: shared.clone(),
            dirty: dirty.clone(),
        });
        start_hotkey_thread(ctx.clone(), hk_map.clone(), rt);
        Self {
            cfg,
            tx,
            shared,
            new_app: String::new(),
            autostart: autostart_enabled(),
            tray: make_tray(ctx),
            tab: Tab::Mixer,
            shared_cfg,
            dirty,
            hk: GlobalHotKeyManager::new().ok(),
            hk_map,
            hk_regs: Vec::new(),
            hk_sig: String::from("~init"),
            hk_msgs: Vec::new(),
            capture: None,
            capture_msg: String::new(),
            preloaded_rate: 0,
            last_trim: Instant::now(),
            pad_search: String::new(),
            pad_group: String::new(),
            pad_page: 0,
            pad_sort: false,
            pad_filter_sig: String::new(),
            pad_remove: None,
            profile_name: String::new(),
            profiles: Config::list_profiles(),
            io_msg: String::new(),
        }
    }

    /// Перерегистрировать глобальные горячие клавиши по текущему конфигу.
    fn sync_hotkeys(&mut self, paused: bool) {
        let Some(m) = self.hk.as_ref() else {
            return;
        };
        if !self.hk_regs.is_empty() {
            let _ = m.unregister_all(&self.hk_regs);
            self.hk_regs.clear();
        }
        let mut map: HashMap<u32, Act> = HashMap::new();
        self.hk_msgs.clear();
        if !paused {
            let mut want: Vec<(String, Act, String)> = Vec::new();
            for (i, x) in self.cfg.sounds.iter().enumerate() {
                if !x.key.is_empty() {
                    want.push((x.key.clone(), Act::Sound(i), x.name.clone()));
                }
            }
            for x in &self.cfg.binds {
                if !x.key.is_empty() {
                    want.push((x.key.clone(), Act::Action(x.action.clone()), action_label(&self.cfg, &x.action)));
                }
            }
            for (key, act, label) in want {
                match key.parse::<HotKey>() {
                    Ok(hk) => {
                        if map.contains_key(&hk.id()) {
                            self.hk_msgs.push(format!("{key} ({label}): сочетание уже занято другим пунктом"));
                            continue;
                        }
                        match m.register(hk) {
                            Ok(()) => {
                                map.insert(hk.id(), act);
                                self.hk_regs.push(hk);
                            }
                            Err(e) => self.hk_msgs.push(format!("{key} ({label}): {e}")),
                        }
                    }
                    Err(e) => self.hk_msgs.push(format!("{key} ({label}): {e}")),
                }
            }
        }
        *self.hk_map.lock().unwrap() = map;
    }

    /// Обработка нажатия при назначении бинда.
    fn handle_capture(&mut self, ctx: &egui::Context) {
        let Some(t) = self.capture else {
            return;
        };
        let events = ctx.input(|i| i.events.clone());
        for ev in events {
            if let egui::Event::Key { key, pressed: true, modifiers, .. } = ev {
                if key == egui::Key::Escape {
                    self.capture = None;
                    self.capture_msg.clear();
                    return;
                }
                let Some(name) = key_name(key) else {
                    continue;
                };
                let mut parts: Vec<String> = Vec::new();
                if modifiers.ctrl {
                    parts.push("Ctrl".into());
                }
                if modifiers.alt {
                    parts.push("Alt".into());
                }
                if modifiers.shift {
                    parts.push("Shift".into());
                }
                if parts.is_empty() && !is_fkey(&name) && !self.cfg.pad_allow_bare {
                    self.capture_msg =
                        "Добавьте Ctrl, Alt или Shift (без них только F1–F24; одиночные клавиши включаются в «Настройки» → Soundpad)".into();
                    continue;
                }
                parts.push(name);
                let combo = parts.join("+");
                match t {
                    Target::Sound(i) => {
                        if let Some(x) = self.cfg.sounds.get_mut(i) {
                            x.key = combo;
                        }
                    }
                    Target::Bind(i) => {
                        if let Some(x) = self.cfg.binds.get_mut(i) {
                            x.key = combo;
                        }
                    }
                }
                self.capture = None;
                self.capture_msg.clear();
                return;
            }
        }
    }

    fn key_button(&mut self, ui: &mut egui::Ui, target: Target, key: &str) {
        let capturing = self.capture == Some(target);
        let label = if capturing {
            "нажмите сочетание…".to_string()
        } else if key.is_empty() {
            "бинд: не задан".to_string()
        } else {
            format!("бинд: {key}")
        };
        let btn = egui::Button::new(RichText::new(label).color(if capturing { Color32::WHITE } else { pal().accent_light }))
            .fill(if capturing { pal().accent } else { Color32::from_rgb(40, 40, 54) });
        if ui.add(btn).clicked() {
            self.capture = if capturing { None } else { Some(target) };
            self.capture_msg.clear();
        }
        if !key.is_empty() && !capturing && ui.small_button("сбросить").clicked() {
            match target {
                Target::Sound(i) => {
                    if let Some(x) = self.cfg.sounds.get_mut(i) {
                        x.key.clear();
                    }
                }
                Target::Bind(i) => {
                    if let Some(x) = self.cfg.binds.get_mut(i) {
                        x.key.clear();
                    }
                }
            }
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui) {
        let info = self.shared.info.lock().unwrap().clone();

        // ── Шапка ──
        ui.horizontal(|ui| {
            ui.label(RichText::new("Voice Mixer").size(26.0).strong());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let (txt, col) = if self.cfg.running { ("Остановить", RED) } else { ("Запустить", pal().accent) };
                let btn = egui::Button::new(RichText::new(txt).strong().color(Color32::WHITE))
                    .fill(col)
                    .rounding(8.0)
                    .min_size(vec2(120.0, 34.0));
                if ui.add(btn).clicked() {
                    self.cfg.running = !self.cfg.running;
                }
                let (st, c) = if self.cfg.running { ("Работает", GREEN) } else { ("Остановлено", pal().dim) };
                status_dot(ui, c, st, true);
            });
        });
        ui.label(RichText::new(&info.out_status).small().color(pal().dim));
        ui.add_space(10.0);

        // ── Вкладки ──
        ui.horizontal(|ui| {
            for (t, label) in [(Tab::Mixer, "Микшер"), (Tab::Pad, "Soundpad"), (Tab::Binds, "Бинды"), (Tab::Settings, "Настройки")] {
                let sel = self.tab == t;
                let txt = RichText::new(label).size(15.0).strong().color(if sel { Color32::WHITE } else { pal().dim });
                let btn = egui::Button::new(txt)
                    .fill(if sel { pal().accent } else { pal().card })
                    .rounding(8.0)
                    .min_size(vec2(96.0, 30.0));
                if ui.add(btn).clicked() {
                    self.tab = t;
                }
            }
        });
        ui.add_space(10.0);
        match self.tab {
            Tab::Mixer => self.ui_mixer(ui),
            Tab::Pad => self.ui_pad(ui),
            Tab::Binds => self.ui_binds(ui),
            Tab::Settings => self.ui_settings(ui),
        }
    }

    fn ui_mixer(&mut self, ui: &mut egui::Ui) {
        let info = self.shared.info.lock().unwrap().clone();
        let shared = self.shared.clone();

        // ── Итоговый уровень ──
        card(ui, "Итоговый уровень", |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("На выходе").small().color(pal().dim));
                meter(ui, shared.out_level(), ui.available_width());
            });
            ui.label(RichText::new("Выбор выхода и прослушивание находятся на вкладке «Настройки».").small().color(pal().dim));
        });

        // ── Микрофоны ──
        card(ui, "Микрофоны · по приоритету", |ui| {
            let n = self.cfg.mics.len();
            let mut swap: Option<(usize, usize)> = None;
            let mut remove: Option<usize> = None;
            for i in 0..n {
                let pat = self.cfg.mics[i].to_lowercase();
                let connected = !pat.is_empty() && info.in_devices.iter().any(|d| d.to_lowercase().contains(&pat));
                let active = !pat.is_empty() && info.active_mic.to_lowercase().contains(&pat);
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("{}.", i + 1)).color(pal().dim));
                    ui.add(TextEdit::singleline(&mut self.cfg.mics[i]).desired_width(200.0));
                    if icon_btn(ui, Ic::Up) && i > 0 {
                        swap = Some((i, i - 1));
                    }
                    if icon_btn(ui, Ic::Down) && i + 1 < n {
                        swap = Some((i, i + 1));
                    }
                    if icon_btn(ui, Ic::Close) {
                        remove = Some(i);
                    }
                    if active {
                        status_dot(ui, GREEN, "в работе", true);
                    } else if connected {
                        status_dot(ui, pal().accent_light, "подключён", true);
                    } else {
                        status_dot(ui, pal().dim, "нет", false);
                    }
                });
            }
            if let Some((a, b)) = swap {
                self.cfg.mics.swap(a, b);
            }
            if let Some(i) = remove {
                self.cfg.mics.remove(i);
            }
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt("mic_add").selected_text("Добавить из подключённых…").show_ui(ui, |ui| {
                    for d in &info.in_devices {
                        if d.to_lowercase().contains("line") {
                            continue;
                        }
                        if ui.selectable_label(false, d).clicked() {
                            self.cfg.mics.push(d.clone());
                        }
                    }
                });
                if ui.small_button("+ пустая строка").clicked() {
                    self.cfg.mics.push(String::new());
                }
            });
            ui.add_space(4.0);
            sliders(ui, &mut self.cfg.mic_volume, &mut self.cfg.mic_delay_ms);
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.cfg.denoise_on, "Шумоподавление");
                ui.add_enabled(
                    self.cfg.denoise_on,
                    egui::Slider::new(&mut self.cfg.denoise_amount, 0.0..=1.0)
                        .text("сила")
                        .custom_formatter(|v, _| format!("{:.0}%", v * 100.0))
                        .custom_parser(|s| s.trim().trim_end_matches('%').parse::<f64>().ok().map(|x| x / 100.0)),
                );
            });
            ui.label(
                RichText::new("Убирает фоновый шум и стук клавиш (RNNoise). Добавляет ~10 мс к задержке микрофона.")
                    .small()
                    .color(pal().dim),
            );
            if self.cfg.denoise_on {
                egui::CollapsingHeader::new(RichText::new("Настройки шумоподавления").color(pal().accent_light)).show(ui, |ui| {
                    ui.checkbox(&mut self.cfg.gate_on, "Гейт по голосу: глушить микрофон, пока вы молчите");
                    ui.add_enabled_ui(self.cfg.gate_on, |ui| {
                        ui.horizontal(|ui| {
                            ui.label("Порог голоса");
                            ui.add(
                                egui::Slider::new(&mut self.cfg.gate_threshold, 0.05..=0.95)
                                    .custom_formatter(|v, _| format!("{:.0}%", v * 100.0))
                                    .custom_parser(|s| s.trim().trim_end_matches('%').parse::<f64>().ok().map(|x| x / 100.0)),
                            );
                        });
                        ui.horizontal(|ui| {
                            ui.label("Удержание");
                            ui.add(egui::Slider::new(&mut self.cfg.gate_hold_ms, 0.0..=600.0).suffix(" мс").step_by(10.0));
                        });
                        ui.horizontal(|ui| {
                            ui.label("Глубина подавления");
                            ui.add(egui::Slider::new(&mut self.cfg.gate_depth_db, 6.0..=80.0).suffix(" дБ").step_by(1.0));
                        });
                    });
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Голос сейчас").small().color(pal().dim));
                        vad_bar(ui, shared.mic.vad_level(), self.cfg.gate_threshold, ui.available_width());
                    });
                    ui.label(
                        RichText::new("Жёлтая черта — порог. Пока полоса ниже неё, гейт закрыт. Подберите порог так, чтобы при разговоре полоса уверенно заходила за черту.")
                            .small()
                            .color(pal().dim),
                    );
                });
            }
            ui.horizontal(|ui| {
                meter(ui, shared.mic.level(), ui.available_width());
            });
        });

        // ── Приложения ──
        card(ui, "Приложения · звук также остаётся в ваших наушниках", |ui| {
            let mut remove: Option<usize> = None;
            let count = self.cfg.apps.len();
            for (i, a) in self.cfg.apps.iter_mut().enumerate() {
                let (status, level) = shared
                    .apps
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|(e, _)| e.eq_ignore_ascii_case(&a.exe))
                    .map(|(_, s)| (s.status.lock().unwrap().clone(), s.level()))
                    .unwrap_or_default();
                ui.horizontal(|ui| {
                    ui.checkbox(&mut a.enabled, "");
                    ui.label(RichText::new(&a.label).strong());
                    ui.label(RichText::new(&a.exe).small().color(pal().dim));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if icon_btn(ui, Ic::Close) {
                            remove = Some(i);
                        }
                        ui.label(RichText::new(&status).small().color(pal().dim));
                    });
                });
                sliders(ui, &mut a.volume, &mut a.delay_ms);
                ui.horizontal(|ui| {
                    meter(ui, level, ui.available_width());
                });
                if i + 1 < count {
                    ui.add_space(4.0);
                    ui.separator();
                }
            }
            if let Some(i) = remove {
                self.cfg.apps.remove(i);
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.add(TextEdit::singleline(&mut self.new_app).hint_text("имя_процесса.exe").desired_width(180.0));
                if ui.button("Добавить").clicked() && !self.new_app.trim().is_empty() {
                    let exe = self.new_app.trim().to_string();
                    add_app(&mut self.cfg, &exe, &exe);
                    self.new_app.clear();
                }
            });
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("Быстро:").small().color(pal().dim));
                for (label, exe) in PRESETS {
                    if ui.small_button(label).clicked() {
                        add_app(&mut self.cfg, label, exe);
                    }
                }
            });
        });
    }

    fn play_sound(&self, i: usize) {
        play_cfg_sound(&self.shared, &self.cfg, i);
    }

    fn ui_pad(&mut self, ui: &mut egui::Ui) {
        let shared = self.shared.clone();
        card(ui, "Soundpad · звуки в микс", |ui| {
            ui.horizontal_wrapped(|ui| {
                if ui.button("Добавить звуки…").clicked() {
                    if let Some(files) = rfd::FileDialog::new().add_filter("Аудио", &soundpad::EXTENSIONS).pick_files() {
                        for f in files {
                            let path = f.to_string_lossy().to_string();
                            if add_sound(&mut self.cfg, &path) {
                                shared.pad.preload(&path);
                            }
                        }
                    }
                }
                if ui.button("Стоп всё").clicked() {
                    shared.pad.stop_all();
                }
                ui.checkbox(&mut self.cfg.pad_exclusive, "Новый звук останавливает предыдущий");
                ui.label(RichText::new(format!("играет: {}", shared.pad.playing())).small().color(pal().dim));
            });
            ui.horizontal(|ui| {
                ui.label("Общая громкость");
                ui.add(
                    egui::Slider::new(&mut self.cfg.pad_volume, 0.0..=3.0)
                        .custom_formatter(|v, _| format!("{:.0}%", v * 100.0))
                        .custom_parser(|s| s.trim().trim_end_matches('%').parse::<f64>().ok().map(|x| x / 100.0)),
                );
            });
            if self.capture.is_some() {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Назначение бинда: нажмите сочетание клавиш").color(YELLOW));
                    if ui.small_button("Отмена").clicked() {
                        self.capture = None;
                        self.capture_msg.clear();
                    }
                });
            }
            ui.label(
                RichText::new(
                    "Перетащите файлы в окно или нажмите «Добавить звуки…» (mp3, wav, ogg, flac, m4a). \
                     Клик по плитке — играть, правый клик — переименовать, бинд, громкость, группа.",
                )
                .small()
                .color(pal().dim),
            );
            let st = shared.pad.status.lock().unwrap().clone();
            if !st.is_empty() {
                ui.label(RichText::new(st).small().color(RED));
            }
            if !self.capture_msg.is_empty() {
                ui.label(RichText::new(&self.capture_msg).small().color(YELLOW));
            }
        });

        let n = self.cfg.sounds.len();
        card(ui, &format!("Звуки · {n}"), |ui| {
            if n == 0 {
                ui.label(RichText::new("Пока пусто.").color(pal().dim));
                return;
            }

            // ── поиск и фильтры ──
            let mut groups: Vec<String> = self
                .cfg
                .sounds
                .iter()
                .map(|s| s.group.trim().to_string())
                .filter(|g| !g.is_empty())
                .collect();
            groups.sort();
            groups.dedup();
            if !self.pad_group.is_empty() && self.pad_group != NO_GROUP && !groups.contains(&self.pad_group) {
                self.pad_group.clear();
            }
            ui.horizontal(|ui| {
                ui.add(
                    TextEdit::singleline(&mut self.pad_search)
                        .hint_text("Поиск: название, группа или бинд")
                        .desired_width(260.0),
                );
                if !self.pad_search.is_empty() && ui.small_button("сбросить").clicked() {
                    self.pad_search.clear();
                }
            });
            ui.horizontal_wrapped(|ui| {
                let sel_text = if self.pad_group.is_empty() {
                    "Все группы".to_string()
                } else if self.pad_group == NO_GROUP {
                    "Без группы".to_string()
                } else {
                    self.pad_group.clone()
                };
                egui::ComboBox::from_id_salt("pad_group").width(150.0).selected_text(sel_text).show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.pad_group, String::new(), "Все группы");
                    ui.selectable_value(&mut self.pad_group, NO_GROUP.to_string(), "Без группы");
                    for g in &groups {
                        ui.selectable_value(&mut self.pad_group, g.clone(), g.as_str());
                    }
                });
                ui.checkbox(&mut self.pad_sort, "по алфавиту");
                ui.selectable_value(&mut self.cfg.pad_view, "Плитки".to_string(), "Плитки");
                ui.selectable_value(&mut self.cfg.pad_view, "Список".to_string(), "Список");
                let pp_text = if self.cfg.pad_per_page == 0 { "все".to_string() } else { format!("по {}", self.cfg.pad_per_page) };
                egui::ComboBox::from_id_salt("pad_pp").width(80.0).selected_text(pp_text).show_ui(ui, |ui| {
                    for v in [12u32, 24, 48, 96, 0] {
                        let label = if v == 0 { "все".to_string() } else { format!("по {v}") };
                        ui.selectable_value(&mut self.cfg.pad_per_page, v, label);
                    }
                });
            });

            // ── какие звуки показывать ──
            let q = self.pad_search.trim().to_lowercase();
            let mut vis: Vec<usize> = (0..n)
                .filter(|&i| {
                    let s = &self.cfg.sounds[i];
                    let g = s.group.trim();
                    let in_group = self.pad_group.is_empty()
                        || (self.pad_group == NO_GROUP && g.is_empty())
                        || g == self.pad_group;
                    in_group
                        && (q.is_empty()
                            || s.name.to_lowercase().contains(&q)
                            || g.to_lowercase().contains(&q)
                            || s.key.to_lowercase().contains(&q))
                })
                .collect();
            if self.pad_sort {
                vis.sort_by_key(|&i| self.cfg.sounds[i].name.to_lowercase());
            }

            // ── страницы ──
            let pp = self.cfg.pad_per_page as usize;
            let fsig = format!("{q}|{}|{}|{pp}", self.pad_group, self.pad_sort);
            if fsig != self.pad_filter_sig {
                self.pad_filter_sig = fsig;
                self.pad_page = 0;
            }
            let pages = if pp == 0 { 1 } else { ((vis.len() + pp - 1) / pp).max(1) };
            if self.pad_page >= pages {
                self.pad_page = pages - 1;
            }
            let (a, b) = if pp == 0 {
                (0, vis.len())
            } else {
                (self.pad_page * pp, ((self.pad_page + 1) * pp).min(vis.len()))
            };
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!("найдено {} · страница {} из {}", vis.len(), self.pad_page + 1, pages))
                        .small()
                        .color(pal().dim),
                );
                if pages > 1 {
                    if ui.add_enabled(self.pad_page > 0, egui::Button::new("Назад")).clicked() {
                        self.pad_page -= 1;
                    }
                    if ui.add_enabled(self.pad_page + 1 < pages, egui::Button::new("Далее")).clicked() {
                        self.pad_page += 1;
                    }
                }
            });
            ui.add_space(4.0);

            let items: Vec<usize> = vis[a..b].to_vec();
            if items.is_empty() {
                ui.label(RichText::new("Ничего не найдено.").color(pal().dim));
            } else if self.cfg.pad_view == "Список" {
                let last = items.len() - 1;
                for (k, &i) in items.iter().enumerate() {
                    self.sound_row(ui, i);
                    if k < last {
                        ui.add_space(2.0);
                        ui.separator();
                    }
                }
            } else {
                let tile_w = 150.0_f32;
                let cols = (((ui.available_width() + 8.0) / (tile_w + 8.0)).floor() as usize).max(1);
                egui::Grid::new("pad_tiles").spacing([8.0, 8.0]).show(ui, |ui| {
                    for (k, &i) in items.iter().enumerate() {
                        self.sound_tile(ui, i, tile_w);
                        if (k + 1) % cols == 0 {
                            ui.end_row();
                        }
                    }
                });
            }
        });

        if let Some(i) = self.pad_remove.take() {
            if i < self.cfg.sounds.len() {
                self.cfg.sounds.remove(i);
            }
            self.capture = None;
        }
        ui.add_space(4.0);
        for m in &self.hk_msgs {
            ui.label(RichText::new(format!("Бинд не работает: {m}")).small().color(YELLOW));
        }
    }

    /// Плитка звука: клик — играть, правый клик — настройки.
    fn sound_tile(&mut self, ui: &mut egui::Ui, i: usize, w: f32) {
        let (path, name, key, group) = {
            let s = &self.cfg.sounds[i];
            (s.path.clone(), s.name.clone(), s.key.clone(), s.group.trim().to_string())
        };
        let playing = self.shared.pad.is_playing(&path);
        let capturing = self.capture == Some(Target::Sound(i));
        let (rect, resp) = ui.allocate_exact_size(vec2(w, 60.0), Sense::click());
        let p = pal();
        let fill = if playing {
            p.accent.linear_multiply(0.35)
        } else if resp.hovered() {
            Color32::from_rgb(50, 50, 68)
        } else {
            Color32::from_rgb(38, 38, 52)
        };
        let border = if capturing {
            YELLOW
        } else if playing {
            p.accent_light
        } else {
            p.border
        };
        {
            let painter = ui.painter();
            painter.rect_filled(rect, 10.0, fill);
            painter.rect_stroke(rect, 10.0, Stroke::new(1.5_f32, border));
            let clip = painter.with_clip_rect(rect.shrink2(vec2(6.0, 3.0)));
            clip.text(
                rect.left_top() + vec2(10.0, 8.0),
                egui::Align2::LEFT_TOP,
                &name,
                egui::FontId::proportional(14.0),
                Color32::WHITE,
            );
            let (sub, sub_col) = if capturing {
                ("нажмите сочетание…".to_string(), YELLOW)
            } else if key.is_empty() {
                ("без бинда".to_string(), p.dim)
            } else {
                (key.clone(), p.accent_light)
            };
            clip.text(
                rect.left_bottom() + vec2(10.0, -7.0),
                egui::Align2::LEFT_BOTTOM,
                sub,
                egui::FontId::proportional(12.0),
                sub_col,
            );
            if !group.is_empty() {
                clip.text(
                    rect.right_bottom() + vec2(-8.0, -7.0),
                    egui::Align2::RIGHT_BOTTOM,
                    &group,
                    egui::FontId::proportional(11.0),
                    p.dim,
                );
            }
        }
        let resp = resp.on_hover_text(format!("{path}\nПравый клик — настройки звука"));
        if resp.clicked() {
            self.play_sound(i);
        }
        resp.context_menu(|ui| {
            self.sound_menu(ui, i);
        });
    }

    fn sound_menu(&mut self, ui: &mut egui::Ui, i: usize) {
        ui.set_min_width(270.0);
        ui.label(RichText::new("Название").small().color(pal().dim));
        ui.add(TextEdit::singleline(&mut self.cfg.sounds[i].name).desired_width(250.0));
        ui.label(RichText::new("Группа").small().color(pal().dim));
        ui.add(TextEdit::singleline(&mut self.cfg.sounds[i].group).hint_text("без группы").desired_width(250.0));
        ui.horizontal(|ui| {
            ui.label("Громкость");
            ui.add(
                egui::Slider::new(&mut self.cfg.sounds[i].volume, 0.0..=3.0)
                    .custom_formatter(|v, _| format!("{:.0}%", v * 100.0))
                    .custom_parser(|s| s.trim().trim_end_matches('%').parse::<f64>().ok().map(|x| x / 100.0)),
            );
        });
        ui.checkbox(&mut self.cfg.sounds[i].looped, "зациклить (повторное нажатие — стоп)");
        ui.horizontal(|ui| {
            ui.label("Режим");
            mode_combo(ui, ("pad_mode_menu", i), &mut self.cfg.sounds[i].exclusive);
        });
        ui.horizontal(|ui| {
            let key = self.cfg.sounds[i].key.clone();
            self.key_button(ui, Target::Sound(i), &key);
        });
        ui.separator();
        ui.horizontal(|ui| {
            if ui.button("Играть").clicked() {
                self.play_sound(i);
            }
            if ui.button("Удалить звук").clicked() {
                self.pad_remove = Some(i);
                ui.close_menu();
            }
        });
    }

    /// Строка звука в режиме «Список».
    fn sound_row(&mut self, ui: &mut egui::Ui, i: usize) {
        ui.horizontal(|ui| {
            ui.add(TextEdit::singleline(&mut self.cfg.sounds[i].name).desired_width(170.0));
            if ui.button("Играть").clicked() {
                self.play_sound(i);
            }
            let key = self.cfg.sounds[i].key.clone();
            self.key_button(ui, Target::Sound(i), &key);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if icon_btn(ui, Ic::Close) {
                    self.pad_remove = Some(i);
                }
            });
        });
        ui.horizontal_wrapped(|ui| {
            ui.label("Громкость");
            ui.add(
                egui::Slider::new(&mut self.cfg.sounds[i].volume, 0.0..=3.0)
                    .custom_formatter(|v, _| format!("{:.0}%", v * 100.0))
                    .custom_parser(|s| s.trim().trim_end_matches('%').parse::<f64>().ok().map(|x| x / 100.0)),
            );
            ui.checkbox(&mut self.cfg.sounds[i].looped, "зациклить");
            mode_combo(ui, ("pad_mode_row", i), &mut self.cfg.sounds[i].exclusive);
            ui.add(TextEdit::singleline(&mut self.cfg.sounds[i].group).hint_text("группа").desired_width(100.0));
        });
        ui.label(RichText::new(&self.cfg.sounds[i].path).small().color(pal().dim));
    }

    fn ui_binds(&mut self, ui: &mut egui::Ui) {
        card(ui, "Бинды · глобальные горячие клавиши", |ui| {
            ui.label(
                RichText::new(
                    "Работают в любых окнах и играх, даже когда программа свёрнута в трей. \
                     Сочетания с Ctrl / Alt / Shift, например Ctrl+Alt+M. Бинды на звуки задаются на вкладке Soundpad.",
                )
                .small()
                .color(pal().dim),
            );
            ui.add_space(4.0);
            let opts = bind_actions(&self.cfg);
            let n = self.cfg.binds.len();
            if n == 0 {
                ui.label(RichText::new("Биндов пока нет.").color(pal().dim));
            }
            let mut remove: Option<usize> = None;
            for i in 0..n {
                ui.horizontal(|ui| {
                    let cur = self.cfg.binds[i].action.clone();
                    egui::ComboBox::from_id_salt(("bind_act", i))
                        .width(250.0)
                        .selected_text(action_label(&self.cfg, &cur))
                        .show_ui(ui, |ui| {
                            for (id, label) in &opts {
                                if ui.selectable_label(cur == *id, label).clicked() {
                                    self.cfg.binds[i].action = id.clone();
                                }
                            }
                        });
                    let key = self.cfg.binds[i].key.clone();
                    self.key_button(ui, Target::Bind(i), &key);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if icon_btn(ui, Ic::Close) {
                            remove = Some(i);
                        }
                    });
                });
            }
            if let Some(i) = remove {
                self.cfg.binds.remove(i);
                self.capture = None;
            }
            ui.add_space(4.0);
            if ui.button("+ Добавить бинд").clicked() {
                self.cfg.binds.push(BindCfg::default());
            }
            if !self.capture_msg.is_empty() {
                ui.label(RichText::new(&self.capture_msg).small().color(YELLOW));
            }
        });
        for m in &self.hk_msgs {
            ui.label(RichText::new(format!("Бинд не работает: {m}")).small().color(YELLOW));
        }
        if self.hk.is_none() {
            ui.label(RichText::new("Глобальные горячие клавиши недоступны в этой системе.").small().color(RED));
        }
    }

    fn ui_settings(&mut self, ui: &mut egui::Ui) {
        let info = self.shared.info.lock().unwrap().clone();
        let shared = self.shared.clone();

        // ── Выход ──
        card(ui, "Выход · виртуальный кабель", |ui| {
            ui.horizontal(|ui| {
                ui.label("Имя содержит:");
                ui.add(TextEdit::singleline(&mut self.cfg.output).desired_width(190.0));
                egui::ComboBox::from_id_salt("out_pick").selected_text("Выбрать…").show_ui(ui, |ui| {
                    for d in &info.out_devices {
                        if ui.selectable_label(false, d).clicked() {
                            self.cfg.output = d.clone();
                        }
                    }
                });
            });
            ui.horizontal(|ui| {
                ui.label(RichText::new("Итоговый уровень").small().color(pal().dim));
                meter(ui, shared.out_level(), ui.available_width());
            });
        });

        // ── Прослушивание ──
        card(ui, "Прослушивание · что уходит в Line 2", |ui| {
            ui.checkbox(&mut self.cfg.monitor_on, "Слышать итоговый микс в этом устройстве");
            ui.horizontal(|ui| {
                ui.label("Устройство:");
                ui.add(TextEdit::singleline(&mut self.cfg.monitor_device).desired_width(190.0));
                egui::ComboBox::from_id_salt("mon_pick").selected_text("Выбрать…").show_ui(ui, |ui| {
                    for d in &info.out_devices {
                        if ui.selectable_label(false, d).clicked() {
                            self.cfg.monitor_device = d.clone();
                        }
                    }
                });
            });
            ui.horizontal(|ui| {
                ui.label("Громкость");
                ui.add(
                    egui::Slider::new(&mut self.cfg.monitor_volume, 0.0..=5.0)
                        .custom_formatter(|v, _| format!("{:.0}%", v * 100.0))
                        .custom_parser(|s| s.trim().trim_end_matches('%').parse::<f64>().ok().map(|x| x / 100.0)),
                );
            });
            if !info.monitor_status.is_empty() {
                ui.label(RichText::new(&info.monitor_status).small().color(pal().dim));
            }
            ui.label(
                RichText::new("Музыка в наушниках играет ещё и напрямую, поэтому здесь она может звучать с эхом — для проверки баланса это нормально.")
                    .small()
                    .color(pal().dim),
            );
        });

        // ── Soundpad ──
        card(ui, "Soundpad", |ui| {
            ui.checkbox(&mut self.cfg.pad_exclusive, "Новый звук останавливает предыдущий (по умолчанию)");
            ui.label(
                RichText::new("Для отдельного звука можно выбрать другой режим: правый клик по плитке или список → «Режим».")
                    .small()
                    .color(pal().dim),
            );
            ui.checkbox(
                &mut self.cfg.pad_allow_bare,
                "Разрешить бинды звуков без Ctrl, Alt и Shift",
            );
            ui.label(
                RichText::new("Одиночная клавиша перестанет работать в других программах, пока бинд задан. F1–F24 всегда разрешены.")
                    .small()
                    .color(pal().dim),
            );
            ui.horizontal(|ui| {
                ui.label("Громкость новых звуков");
                ui.add(
                    egui::Slider::new(&mut self.cfg.pad_default_volume, 0.0..=3.0)
                        .custom_formatter(|v, _| format!("{:.0}%", v * 100.0))
                        .custom_parser(|s| s.trim().trim_end_matches('%').parse::<f64>().ok().map(|x| x / 100.0)),
                );
            });
        });

        card(ui, "Тема оформления", |ui| {
            ui.horizontal_wrapped(|ui| {
                for (name, p) in THEMES.iter() {
                    let sel = self.cfg.theme == *name;
                    let (rect, resp) = ui.allocate_exact_size(vec2(112.0, 34.0), Sense::click());
                    let painter = ui.painter();
                    painter.rect_filled(rect, 8.0, p.card);
                    painter.rect_stroke(rect, 8.0, Stroke::new(if sel { 2.0_f32 } else { 1.0_f32 }, if sel { p.accent_light } else { p.border }));
                    painter.circle_filled(rect.left_center() + vec2(18.0, 0.0), 8.0, p.accent);
                    painter.text(
                        rect.left_center() + vec2(34.0, 0.0),
                        egui::Align2::LEFT_CENTER,
                        *name,
                        egui::FontId::proportional(14.0),
                        Color32::WHITE,
                    );
                    if resp.clicked() {
                        self.cfg.theme = name.to_string();
                    }
                }
            });
        });

        card(ui, "Профили и конфиги", |ui| {
            ui.label(
                RichText::new(
                    "Профиль — сохранённый набор всех настроек: микрофоны, приложения, звуки, бинды, тема. \
                     Конфиг можно экспортировать в файл и поделиться им.",
                )
                .small()
                .color(pal().dim),
            );
            ui.horizontal(|ui| {
                ui.add(TextEdit::singleline(&mut self.profile_name).hint_text("имя профиля").desired_width(180.0));
                if ui.button("Сохранить как профиль").clicked() {
                    match self.cfg.save_profile(&self.profile_name) {
                        Ok(()) => {
                            self.io_msg = format!("Профиль «{}» сохранён", self.profile_name.trim());
                            self.profiles = Config::list_profiles();
                        }
                        Err(e) => self.io_msg = format!("Не удалось сохранить: {e}"),
                    }
                }
            });
            let mut load: Option<String> = None;
            let mut del: Option<String> = None;
            for p in &self.profiles {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(p).strong());
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if icon_btn(ui, Ic::Close) {
                            del = Some(p.clone());
                        }
                        if ui.button("Загрузить").clicked() {
                            load = Some(p.clone());
                        }
                    });
                });
            }
            if let Some(name) = load {
                match Config::load_profile(&name) {
                    Ok(mut c) => {
                        c.running = self.cfg.running;
                        self.cfg = c;
                        self.io_msg = format!("Загружен профиль «{name}»");
                    }
                    Err(e) => self.io_msg = format!("Ошибка: {e}"),
                }
            }
            if let Some(name) = del {
                Config::delete_profile(&name);
                self.profiles = Config::list_profiles();
            }
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if ui.button("Экспорт в файл…").clicked() {
                    if let Some(path) = rfd::FileDialog::new()
                        .set_file_name("voice-mixer-config.toml")
                        .add_filter("Конфиг Voice Mixer", &["toml"])
                        .save_file()
                    {
                        let res = self.cfg.to_toml_string().and_then(|s| std::fs::write(&path, s).map_err(|e| e.to_string()));
                        self.io_msg = match res {
                            Ok(()) => format!("Экспортировано: {}", path.display()),
                            Err(e) => format!("Не удалось экспортировать: {e}"),
                        };
                    }
                }
                if ui.button("Импорт из файла…").clicked() {
                    if let Some(path) = rfd::FileDialog::new().add_filter("Конфиг Voice Mixer", &["toml"]).pick_file() {
                        let res = std::fs::read_to_string(&path).map_err(|e| e.to_string()).and_then(|s| Config::from_toml_str(&s));
                        match res {
                            Ok(mut c) => {
                                c.running = self.cfg.running;
                                self.cfg = c;
                                self.io_msg = "Конфиг импортирован".into();
                            }
                            Err(e) => self.io_msg = format!("Не удалось импортировать: {e}"),
                        }
                    }
                }
            });
            if !self.io_msg.is_empty() {
                ui.label(RichText::new(&self.io_msg).small().color(pal().accent_light));
            }
        });

        card(ui, "Диагностика", |ui| {
            let (ws, private) = proc_mem_mb();
            ui.label(format!("Память процесса: {ws} МБ, из них приватная {private} МБ"));
            ui.label(format!(
                "Кэш звуков Soundpad: {:.0} МБ из {} МБ",
                self.shared.pad.cache_bytes() as f64 / 1_048_576.0,
                192
            ));
            ui.horizontal(|ui| {
                if ui.button("Очистить кэш звуков").clicked() {
                    self.shared.pad.clear_cache();
                }
                if ui.button("Освободить память").clicked() {
                    trim_memory();
                }
            });
            ui.label(
                RichText::new("Приватная память показывает реальное потребление. Если она постоянно растёт, создайте Issue на GitHub.")
                    .small()
                    .color(pal().dim),
            );
        });

        // ── Настройки ──
        card(ui, "Настройки", |ui| {
            if ui.checkbox(&mut self.autostart, "Запускать вместе с Windows (свёрнутым в трей)").changed() {
                set_autostart(self.autostart);
            }
            ui.checkbox(&mut self.cfg.tray_on_close, "Закрытие окна сворачивает в трей");
            ui.label(
                RichText::new("Конфиг: %APPDATA%\\VoiceMixer\\config.toml").small().color(pal().dim),
            );
        });
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if ctx.input(|i| i.viewport().close_requested()) && self.cfg.tray_on_close && self.tray.is_some() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            VISIBLE.store(false, Ordering::Relaxed);
            trim_memory();
            self.last_trim = Instant::now();
        }

        // изменения, сделанные горячими клавишами
        if self.dirty.swap(false, Ordering::Relaxed) {
            self.cfg = self.shared_cfg.lock().unwrap().clone();
        }
        self.handle_capture(ctx);

        // файлы, перетащенные в окно, добавляются в Soundpad
        let dropped = ctx.input(|i| i.raw.dropped_files.clone());
        for f in dropped {
            if let Some(p) = f.path {
                let path = p.to_string_lossy().to_string();
                if add_sound(&mut self.cfg, &path) {
                    self.shared.pad.preload(&path);
                    self.tab = Tab::Pad;
                }
            }
        }

        // заранее декодируем звуки, когда стала известна частота вывода
        let rate = self.shared.pad.rate.load(std::sync::atomic::Ordering::Relaxed);
        if rate != 0 && rate != self.preloaded_rate {
            self.preloaded_rate = rate;
            for s in &self.cfg.sounds {
                self.shared.pad.preload(&s.path);
            }
        }

        let minimized = ctx.input(|i| i.viewport().minimized).unwrap_or(false);
        let visible = VISIBLE.load(Ordering::Relaxed) && !minimized;
        let before = self.cfg.clone();
        if visible {
            egui::CentralPanel::default()
                .frame(egui::Frame::none().fill(pal().bg).inner_margin(16.0))
                .show(ctx, |ui| {
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        self.ui(ui);
                    });
                });
        }
        if self.cfg != before {
            self.cfg.save();
            let _ = self.tx.send(self.cfg.clone());
            *self.shared_cfg.lock().unwrap() = self.cfg.clone();
            if self.cfg.theme != before.theme {
                set_theme(&self.cfg.theme);
                apply_style(ctx);
            }
            if self.cfg.sounds != before.sounds {
                let paths: Vec<String> = self.cfg.sounds.iter().map(|s| s.path.clone()).collect();
                self.shared.pad.retain_paths(&paths);
            }
        }

        // горячие клавиши: пока назначаем бинд, все снимаются, чтобы можно было нажать уже занятое сочетание
        let sig = if self.capture.is_some() { String::from("~capture") } else { signature(&self.cfg) };
        if sig != self.hk_sig {
            self.hk_sig = sig.clone();
            self.sync_hotkeys(sig == "~capture");
        }

        // Частота перерисовки: быстро только когда окно открыто и есть что анимировать.
        let playing = self.shared.pad.playing() > 0;
        let ms = if !visible {
            if self.last_trim.elapsed() >= Duration::from_secs(60) {
                trim_memory();
                self.last_trim = Instant::now();
            }
            1500
        } else if self.tab == Tab::Mixer || self.capture.is_some() || playing {
            60
        } else {
            250
        };
        ctx.request_repaint_after(Duration::from_millis(ms));
    }
}

fn card(ui: &mut egui::Ui, title: &str, add: impl FnOnce(&mut egui::Ui)) {
    let r = egui::Frame::none()
        .fill(pal().card)
        .rounding(12.0)
        .stroke(Stroke::new(1.0_f32, pal().border))
        .inner_margin(egui::Margin { left: 18.0, right: 14.0, top: 14.0, bottom: 14.0 })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(title).size(15.0).strong().color(pal().accent_light));
            ui.add_space(6.0);
            add(ui);
        });
    // акцентная полоска слева от заголовка
    let rc = r.response.rect;
    ui.painter().rect_filled(
        egui::Rect::from_min_size(egui::pos2(rc.left() + 7.0, rc.top() + 14.0), vec2(3.0, 18.0)),
        2.0,
        pal().accent,
    );
    ui.add_space(10.0);
}

fn vad_bar(ui: &mut egui::Ui, v: f32, thr: f32, width: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(width.max(20.0), 8.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 4.0, Color32::from_rgb(38, 38, 50));
    let mut fill = rect;
    fill.set_width(rect.width() * v.clamp(0.0, 1.0));
    p.rect_filled(fill, 4.0, if v >= thr { GREEN } else { pal().dim });
    let x = rect.left() + rect.width() * thr.clamp(0.0, 1.0);
    p.line_segment([egui::pos2(x, rect.top() - 2.0), egui::pos2(x, rect.bottom() + 2.0)], Stroke::new(2.0_f32, YELLOW));
}

fn meter(ui: &mut egui::Ui, level: f32, width: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(width.max(20.0), 8.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 4.0, Color32::from_rgb(38, 38, 50));
    let l = level.clamp(0.0, 1.0).sqrt();
    let mut fill = rect;
    fill.set_width(rect.width() * l);
    let col = if l > 0.9 { RED } else if l > 0.7 { YELLOW } else { GREEN };
    p.rect_filled(fill, 4.0, col);
}

fn sliders(ui: &mut egui::Ui, volume: &mut f32, delay: &mut f32) {
    ui.horizontal(|ui| {
        ui.label("Громкость");
        ui.add(
            egui::Slider::new(volume, 0.0..=5.0)
                .custom_formatter(|v, _| format!("{:.0}%", v * 100.0))
                .custom_parser(|s| s.trim().trim_end_matches('%').parse::<f64>().ok().map(|x| x / 100.0)),
        );
        ui.label("Задержка");
        ui.add(egui::Slider::new(delay, 0.0..=300.0).suffix(" мс").step_by(1.0));
    });
}


// Иконки рисуются вручную: в шрифте egui нет стрелок и крестика (получались квадраты).
#[derive(Clone, Copy)]
enum Ic {
    Up,
    Down,
    Close,
}

fn icon_btn(ui: &mut egui::Ui, ic: Ic) -> bool {
    let (rect, resp) = ui.allocate_exact_size(vec2(28.0, 22.0), Sense::click());
    let bg = if resp.hovered() { Color32::from_rgb(56, 56, 76) } else { Color32::from_rgb(40, 40, 54) };
    let p = ui.painter();
    p.rect_filled(rect, 6.0, bg);
    let c = rect.center();
    let col = if matches!(ic, Ic::Close) && resp.hovered() { RED } else { Color32::from_rgb(210, 210, 225) };
    let st = Stroke::new(1.6_f32, col);
    match ic {
        Ic::Up => {
            p.line_segment([c + vec2(-4.5, 2.0), c + vec2(0.0, -2.5)], st);
            p.line_segment([c + vec2(0.0, -2.5), c + vec2(4.5, 2.0)], st);
        }
        Ic::Down => {
            p.line_segment([c + vec2(-4.5, -2.0), c + vec2(0.0, 2.5)], st);
            p.line_segment([c + vec2(0.0, 2.5), c + vec2(4.5, -2.0)], st);
        }
        Ic::Close => {
            p.line_segment([c + vec2(-3.5, -3.5), c + vec2(3.5, 3.5)], st);
            p.line_segment([c + vec2(-3.5, 3.5), c + vec2(3.5, -3.5)], st);
        }
    }
    resp.clicked()
}

fn status_dot(ui: &mut egui::Ui, color: Color32, text: &str, filled: bool) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 5.0;
        let (r, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
        if filled {
            ui.painter().circle_filled(r.center(), 4.0, color);
        } else {
            ui.painter().circle_stroke(r.center(), 3.5, Stroke::new(1.3_f32, color));
        }
        ui.label(RichText::new(text).color(color));
    });
}

fn add_app(cfg: &mut Config, label: &str, exe: &str) {
    let mut exe = exe.trim().to_string();
    if !exe.to_lowercase().ends_with(".exe") {
        exe.push_str(".exe");
    }
    if cfg.apps.iter().any(|a| a.exe.eq_ignore_ascii_case(&exe)) {
        return;
    }
    cfg.apps.push(AppCfg { label: label.trim().to_string(), exe, ..Default::default() });
}

fn apply_style(ctx: &egui::Context) {
    let mut v = egui::Visuals::dark();
    v.panel_fill = pal().bg;
    v.window_fill = pal().card;
    v.extreme_bg_color = Color32::from_rgb(12, 12, 16);
    v.selection.bg_fill = pal().accent;
    let base = Color32::from_rgb(40, 40, 54);
    let hover = Color32::from_rgb(56, 56, 76);
    v.widgets.inactive.bg_fill = base;
    v.widgets.inactive.weak_bg_fill = base;
    v.widgets.hovered.bg_fill = hover;
    v.widgets.hovered.weak_bg_fill = hover;
    v.widgets.active.bg_fill = pal().accent;
    v.widgets.active.weak_bg_fill = pal().accent;
    for w in [&mut v.widgets.noninteractive, &mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active] {
        w.rounding = Rounding::same(8.0);
    }
    ctx.set_visuals(v);
    ctx.style_mut(|s| {
        s.spacing.item_spacing = vec2(10.0, 8.0);
        s.spacing.slider_width = 110.0;
        s.spacing.button_padding = vec2(10.0, 5.0);
    });
}

// ───────────────────────────── Трей ─────────────────────────────

/// Окно видно на экране (не в трее и не свёрнуто). Когда нет, интерфейс не рисуется.
static VISIBLE: AtomicBool = AtomicBool::new(true);

#[repr(C)]
struct ProcMemCounters {
    cb: u32,
    page_fault_count: u32,
    peak_working_set: usize,
    working_set: usize,
    quota_peak_paged: usize,
    quota_paged: usize,
    quota_peak_nonpaged: usize,
    quota_nonpaged: usize,
    pagefile: usize,
    peak_pagefile: usize,
}

#[link(name = "kernel32")]
extern "system" {
    fn GetCurrentProcess() -> isize;
    fn K32EmptyWorkingSet(process: isize) -> i32;
    fn K32GetProcessMemoryInfo(process: isize, counters: *mut ProcMemCounters, cb: u32) -> i32;
}

/// Вернуть системе неиспользуемые страницы памяти (в диспетчере задач потребление заметно падает).
fn trim_memory() {
    unsafe {
        K32EmptyWorkingSet(GetCurrentProcess());
    }
}

/// (рабочий набор, приватная память) процесса в МБ.
fn proc_mem_mb() -> (usize, usize) {
    unsafe {
        let mut c = ProcMemCounters {
            cb: std::mem::size_of::<ProcMemCounters>() as u32,
            page_fault_count: 0,
            peak_working_set: 0,
            working_set: 0,
            quota_peak_paged: 0,
            quota_paged: 0,
            quota_peak_nonpaged: 0,
            quota_nonpaged: 0,
            pagefile: 0,
            peak_pagefile: 0,
        };
        let cb = c.cb;
        if K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, cb) != 0 {
            (c.working_set / 1_048_576, c.pagefile / 1_048_576)
        } else {
            (0, 0)
        }
    }
}

fn show_window(ctx: &egui::Context) {
    VISIBLE.store(true, Ordering::Relaxed);
    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    ctx.request_repaint();
}

fn make_tray(ctx: &egui::Context) -> Option<TrayIcon> {
    let menu = Menu::new();
    let show = MenuItem::new("Показать", true, None);
    let quit = MenuItem::new("Выход", true, None);
    menu.append_items(&[&show, &quit]).ok()?;
    let show_id = show.id().clone();
    let quit_id = quit.id().clone();

    let c = ctx.clone();
    MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
        if e.id == show_id {
            show_window(&c);
        } else if e.id == quit_id {
            std::process::exit(0);
        }
    }));
    let c2 = ctx.clone();
    TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
        if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = e {
            show_window(&c2);
        }
    }));

    TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .with_tooltip("Voice Mixer")
        .with_icon(make_icon())
        .build()
        .ok()
}

fn make_icon() -> Icon {
    Icon::from_rgba(icon_rgba(32), 32, 32).expect("icon")
}

/// Иконка: фиолетовый круг с тремя белыми «столбиками» уровня. Рисуется в любом размере (сглаживание 3×3).
fn icon_rgba(n: usize) -> Vec<u8> {
    let bars: [(f32, f32, f32); 3] = [(9.5, 13.5, 7.0), (14.5, 18.5, 11.0), (19.5, 23.5, 5.0)];
    let k = 32.0 / n as f32;
    let mut out = Vec::with_capacity(n * n * 4);
    for y in 0..n {
        for x in 0..n {
            let (mut r, mut g, mut b, mut cov) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
            for sy in 0..3 {
                for sx in 0..3 {
                    let fx = (x as f32 + (sx as f32 + 0.5) / 3.0) * k;
                    let fy = (y as f32 + (sy as f32 + 0.5) / 3.0) * k;
                    let (dx, dy) = (fx - 16.0, fy - 16.0);
                    if (dx * dx + dy * dy).sqrt() < 14.5 {
                        let in_bar = bars.iter().any(|(a, c, h)| fx >= *a && fx < *c && dy.abs() < *h);
                        let col = if in_bar { [255.0, 255.0, 255.0] } else { [124.0, 92.0, 255.0] };
                        r += col[0];
                        g += col[1];
                        b += col[2];
                        cov += 1.0;
                    }
                }
            }
            if cov > 0.0 {
                out.extend_from_slice(&[(r / cov) as u8, (g / cov) as u8, (b / cov) as u8, (cov / 9.0 * 255.0) as u8]);
            } else {
                out.extend_from_slice(&[0, 0, 0, 0]);
            }
        }
    }
    out
}

// ───────────────────────────── Автозапуск ─────────────────────────────

fn autostart_enabled() -> bool {
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(RUN_KEY)
        .and_then(|k| k.get_value::<String, _>("VoiceMixer"))
        .is_ok()
}

fn set_autostart(on: bool) {
    if let Ok((k, _)) = RegKey::predef(HKEY_CURRENT_USER).create_subkey(RUN_KEY) {
        if on {
            if let Ok(exe) = std::env::current_exe() {
                let _ = k.set_value("VoiceMixer", &format!("\"{}\" --minimized", exe.display()));
            }
        } else {
            let _ = k.delete_value("VoiceMixer");
        }
    }
}
