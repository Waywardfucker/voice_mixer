#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod engine;

use config::{AppCfg, Config};
use eframe::egui::{self, Align, Color32, Layout, RichText, Rounding, Sense, Stroke, TextEdit, vec2};
use engine::Shared;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;
use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use winreg::{enums::HKEY_CURRENT_USER, RegKey};

const BG: Color32 = Color32::from_rgb(17, 17, 23);
const CARD: Color32 = Color32::from_rgb(26, 26, 36);
const BORDER: Color32 = Color32::from_rgb(44, 44, 60);
const ACCENT: Color32 = Color32::from_rgb(124, 92, 255);
const ACCENT_LIGHT: Color32 = Color32::from_rgb(170, 150, 255);
const DIM: Color32 = Color32::from_rgb(140, 140, 160);
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

    let shared = Shared::new();
    let tx = engine::spawn(shared.clone(), cfg.clone());

    let hidden = minimized && cfg.tray_on_close;
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Voice Mixer")
            .with_inner_size([620.0, 760.0])
            .with_min_inner_size([540.0, 480.0])
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

struct App {
    cfg: Config,
    tx: Sender<Config>,
    shared: Arc<Shared>,
    new_app: String,
    autostart: bool,
    tray: Option<TrayIcon>,
}

impl App {
    fn new(ctx: &egui::Context, cfg: Config, tx: Sender<Config>, shared: Arc<Shared>) -> Self {
        Self { cfg, tx, shared, new_app: String::new(), autostart: autostart_enabled(), tray: make_tray(ctx) }
    }

    fn ui(&mut self, ui: &mut egui::Ui) {
        let info = self.shared.info.lock().unwrap().clone();
        let shared = self.shared.clone();

        // ── Шапка ──
        ui.horizontal(|ui| {
            ui.label(RichText::new("Voice Mixer").size(26.0).strong());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let (txt, col) = if self.cfg.running { ("Остановить", RED) } else { ("Запустить", ACCENT) };
                let btn = egui::Button::new(RichText::new(txt).strong().color(Color32::WHITE))
                    .fill(col)
                    .rounding(8.0)
                    .min_size(vec2(120.0, 34.0));
                if ui.add(btn).clicked() {
                    self.cfg.running = !self.cfg.running;
                }
                let (st, c) = if self.cfg.running { ("Работает", GREEN) } else { ("Остановлено", DIM) };
                status_dot(ui, c, st, true);
            });
        });
        ui.label(RichText::new(&info.out_status).small().color(DIM));
        ui.add_space(12.0);

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
                ui.label(RichText::new("Итоговый уровень").small().color(DIM));
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
                ui.label(RichText::new(&info.monitor_status).small().color(DIM));
            }
            ui.label(
                RichText::new("Музыка в наушниках играет ещё и напрямую, поэтому здесь она может звучать с эхом — для проверки баланса это нормально.")
                    .small()
                    .color(DIM),
            );
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
                    ui.label(RichText::new(format!("{}.", i + 1)).color(DIM));
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
                        status_dot(ui, ACCENT_LIGHT, "подключён", true);
                    } else {
                        status_dot(ui, DIM, "нет", false);
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
                    .color(DIM),
            );
            if self.cfg.denoise_on {
                egui::CollapsingHeader::new(RichText::new("Настройки шумоподавления").color(ACCENT_LIGHT)).show(ui, |ui| {
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
                        ui.label(RichText::new("Голос сейчас").small().color(DIM));
                        vad_bar(ui, shared.mic.vad_level(), self.cfg.gate_threshold, ui.available_width());
                    });
                    ui.label(
                        RichText::new("Жёлтая черта — порог. Пока полоса ниже неё, гейт закрыт. Подберите порог так, чтобы при разговоре полоса уверенно заходила за черту.")
                            .small()
                            .color(DIM),
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
                    ui.label(RichText::new(&a.exe).small().color(DIM));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if icon_btn(ui, Ic::Close) {
                            remove = Some(i);
                        }
                        ui.label(RichText::new(&status).small().color(DIM));
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
                ui.label(RichText::new("Быстро:").small().color(DIM));
                for (label, exe) in PRESETS {
                    if ui.small_button(label).clicked() {
                        add_app(&mut self.cfg, label, exe);
                    }
                }
            });
        });

        // ── Настройки ──
        card(ui, "Настройки", |ui| {
            if ui.checkbox(&mut self.autostart, "Запускать вместе с Windows (свёрнутым в трей)").changed() {
                set_autostart(self.autostart);
            }
            ui.checkbox(&mut self.cfg.tray_on_close, "Закрытие окна сворачивает в трей");
            ui.label(
                RichText::new(format!("Конфиг: {}", Config::path().display())).small().color(DIM),
            );
        });
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if ctx.input(|i| i.viewport().close_requested()) && self.cfg.tray_on_close && self.tray.is_some() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }

        let before = self.cfg.clone();
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(BG).inner_margin(16.0))
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    self.ui(ui);
                });
            });
        if self.cfg != before {
            self.cfg.save();
            let _ = self.tx.send(self.cfg.clone());
        }
        ctx.request_repaint_after(Duration::from_millis(60));
    }
}

// ───────────────────────────── Виджеты ─────────────────────────────

fn card(ui: &mut egui::Ui, title: &str, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::none()
        .fill(CARD)
        .rounding(12.0)
        .stroke(Stroke::new(1.0_f32, BORDER))
        .inner_margin(14.0)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(title).size(15.0).strong().color(ACCENT_LIGHT));
            ui.add_space(6.0);
            add(ui);
        });
    ui.add_space(10.0);
}

fn vad_bar(ui: &mut egui::Ui, v: f32, thr: f32, width: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(width.max(20.0), 8.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 4.0, Color32::from_rgb(38, 38, 50));
    let mut fill = rect;
    fill.set_width(rect.width() * v.clamp(0.0, 1.0));
    p.rect_filled(fill, 4.0, if v >= thr { GREEN } else { DIM });
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
    v.panel_fill = BG;
    v.window_fill = CARD;
    v.extreme_bg_color = Color32::from_rgb(12, 12, 16);
    v.selection.bg_fill = ACCENT;
    let base = Color32::from_rgb(40, 40, 54);
    let hover = Color32::from_rgb(56, 56, 76);
    v.widgets.inactive.bg_fill = base;
    v.widgets.inactive.weak_bg_fill = base;
    v.widgets.hovered.bg_fill = hover;
    v.widgets.hovered.weak_bg_fill = hover;
    v.widgets.active.bg_fill = ACCENT;
    v.widgets.active.weak_bg_fill = ACCENT;
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

fn show_window(ctx: &egui::Context) {
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
    let n = 32usize;
    let bars: [(f32, f32, f32); 3] = [(9.0, 13.0, 7.0), (14.0, 18.0, 11.0), (19.0, 23.0, 5.0)];
    let mut rgba = Vec::with_capacity(n * n * 4);
    for y in 0..n {
        for x in 0..n {
            let (fx, fy) = (x as f32, y as f32);
            let (dx, dy) = (fx - 15.5, fy - 15.5);
            let d = (dx * dx + dy * dy).sqrt();
            if d < 14.0 {
                let in_bar = bars.iter().any(|(a, b, h)| fx >= *a && fx < *b && dy.abs() < *h);
                if in_bar {
                    rgba.extend_from_slice(&[255, 255, 255, 255]);
                } else {
                    rgba.extend_from_slice(&[124, 92, 255, 255]);
                }
            } else {
                rgba.extend_from_slice(&[0, 0, 0, 0]);
            }
        }
    }
    Icon::from_rgba(rgba, n as u32, n as u32).expect("icon")
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
