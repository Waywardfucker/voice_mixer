//! Аудио-движок: захват микрофона (cpal), захват звука отдельных процессов (WASAPI
//! process loopback), микширование и вывод в виртуальный кабель (cpal).

use crate::config::Config;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample, Stream, StreamConfig};
use std::collections::{HashSet, VecDeque};
use std::error::Error;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use sysinfo::{Pid, ProcessesToUpdate, System};

/// Запас на джиттер, одинаковый для всех источников (относительная задержка не меняется).
const JITTER_MS: f32 = 20.0;
/// Максимум накопленного звука в буфере источника (в сэмплах, ~2 с при 48 кГц стерео).
const MAX_BUF_SAMPLES: usize = 192_000;

// ───────────────────────────── Источник ─────────────────────────────

pub struct Source {
    buf: Mutex<VecDeque<f32>>,
    volume: AtomicU32,
    delay_ms: AtomicU32,
    enabled: AtomicBool,
    primed: AtomicBool,
    peak: AtomicU32,
    pub denoise_on: AtomicBool,
    pub denoise_amt: AtomicU32,
    pub gate_on: AtomicBool,
    pub gate_thr: AtomicU32,
    pub gate_hold: AtomicU32,
    pub gate_depth: AtomicU32,
    pub vad: AtomicU32,
    pub status: Mutex<String>,
}

impl Source {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            buf: Mutex::new(VecDeque::new()),
            volume: AtomicU32::new(1.0f32.to_bits()),
            delay_ms: AtomicU32::new(0.0f32.to_bits()),
            enabled: AtomicBool::new(true),
            primed: AtomicBool::new(false),
            peak: AtomicU32::new(0),
            denoise_on: AtomicBool::new(false),
            denoise_amt: AtomicU32::new(1.0f32.to_bits()),
            gate_on: AtomicBool::new(false),
            gate_thr: AtomicU32::new(0.5f32.to_bits()),
            gate_hold: AtomicU32::new(200.0f32.to_bits()),
            gate_depth: AtomicU32::new(40.0f32.to_bits()),
            vad: AtomicU32::new(0),
            status: Mutex::new(String::new()),
        })
    }

    pub fn set(&self, volume: f32, delay_ms: f32, enabled: bool) {
        let old = f32::from_bits(self.delay_ms.load(Relaxed));
        self.volume.store(volume.to_bits(), Relaxed);
        self.delay_ms.store(delay_ms.to_bits(), Relaxed);
        self.enabled.store(enabled, Relaxed);
        if (old - delay_ms).abs() > 0.5 {
            // новая задержка применится после повторного наполнения буфера
            self.primed.store(false, Relaxed);
        }
    }

    pub fn level(&self) -> f32 {
        f32::from_bits(self.peak.load(Relaxed))
    }

    pub fn vad_level(&self) -> f32 {
        f32::from_bits(self.vad.load(Relaxed))
    }

    /// Положить стерео-сэмплы (interleaved) на частоте устройства вывода.
    pub fn push(&self, samples: &[f32]) {
        let mut b = self.buf.lock().unwrap();
        b.extend(samples.iter().copied());
        if b.len() > MAX_BUF_SAMPLES {
            let extra = b.len() - MAX_BUF_SAMPLES;
            b.drain(..extra);
        }
    }

    /// Домешать источник в `out` (стерео interleaved).
    fn mix_into(&self, out: &mut [f32], rate: u32) {
        let vol = if self.enabled.load(Relaxed) { f32::from_bits(self.volume.load(Relaxed)) } else { 0.0 };
        let delay = f32::from_bits(self.delay_ms.load(Relaxed));
        let target = ((delay + JITTER_MS) * rate as f32 / 1000.0) as usize * 2;
        let mut b = self.buf.lock().unwrap();
        let mut pk = 0.0f32;

        if !self.primed.load(Relaxed) && b.len() >= target {
            self.primed.store(true, Relaxed);
        }
        if self.primed.load(Relaxed) {
            // дрейф часов: если накопилось сильно больше цели — отбрасываем лишнее
            let max = target + (60.0 * rate as f32 / 1000.0) as usize * 2;
            if b.len() > max {
                let mut drop = b.len() - target;
                drop -= drop % 2;
                b.drain(..drop);
            }
            let n = out.len().min(b.len());
            for o in out.iter_mut().take(n) {
                let v = b.pop_front().unwrap_or(0.0) * vol;
                *o += v;
                pk = pk.max(v.abs());
            }
            if n < out.len() {
                self.primed.store(false, Relaxed); // буфер опустел — ждём наполнения
            }
        }
        let old = f32::from_bits(self.peak.load(Relaxed));
        self.peak.store(pk.max(old * 0.9).to_bits(), Relaxed);
    }
}

// ───────────────────────────── Ресемплер ─────────────────────────────

struct Resampler {
    step: f64,
    pos: f64,
    prev: [f32; 2],
    pass: bool,
}

impl Resampler {
    fn new(in_rate: u32, out_rate: u32) -> Self {
        Self { step: in_rate as f64 / out_rate as f64, pos: 0.0, prev: [0.0; 2], pass: in_rate == out_rate }
    }

    /// Линейный ресемплинг стерео interleaved.
    fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.pass {
            out.extend_from_slice(input);
            return;
        }
        for fr in input.chunks_exact(2) {
            while self.pos < 1.0 {
                let t = self.pos as f32;
                out.push(self.prev[0] * (1.0 - t) + fr[0] * t);
                out.push(self.prev[1] * (1.0 - t) + fr[1] * t);
                self.pos += self.step;
            }
            self.pos -= 1.0;
            self.prev = [fr[0], fr[1]];
        }
    }
}

// ───────────────────────────── Общее состояние для GUI ─────────────────────────────

#[derive(Clone, Default)]
pub struct Info {
    pub in_devices: Vec<String>,
    pub out_devices: Vec<String>,
    pub active_mic: String,
    pub out_status: String,
    pub monitor_status: String,
}

pub struct Shared {
    pub mic: Arc<Source>,
    /// (exe, источник) в порядке конфигурации
    pub apps: Mutex<Vec<(String, Arc<Source>)>>,
    pub info: Mutex<Info>,
    pub out_peak: AtomicU32,
}

impl Shared {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            mic: Source::new(),
            apps: Mutex::new(Vec::new()),
            info: Mutex::new(Info::default()),
            out_peak: AtomicU32::new(0),
        })
    }
    pub fn out_level(&self) -> f32 {
        f32::from_bits(self.out_peak.load(Relaxed))
    }
}


// ───────────────────────────── Прослушивание ─────────────────────────────

pub struct Monitor {
    buf: Mutex<VecDeque<f32>>,
    active: AtomicBool,
    rate: AtomicU32,
    volume: AtomicU32,
    primed: AtomicBool,
}

impl Monitor {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            buf: Mutex::new(VecDeque::new()),
            active: AtomicBool::new(false),
            rate: AtomicU32::new(0),
            volume: AtomicU32::new(0.7f32.to_bits()),
            primed: AtomicBool::new(false),
        })
    }

    fn push(&self, s: &[f32]) {
        let mut b = self.buf.lock().unwrap();
        b.extend(s.iter().copied());
        if b.len() > 96_000 {
            let extra = b.len() - 96_000;
            b.drain(..extra);
        }
    }
}

// ───────────────────────────── Захват процесса (WASAPI) ─────────────────────────────

struct Loopback {
    pid: u32,
    stop: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Loopback {
    fn start(pid: u32, rate: u32, src: Arc<Source>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let (s2, d2) = (stop.clone(), done.clone());
        let handle = std::thread::spawn(move || {
            let _ = wasapi::initialize_mta();
            if let Err(e) = run_loopback(pid, rate, &src, &s2) {
                *src.status.lock().unwrap() = format!("ошибка захвата: {e}");
            }
            d2.store(true, Relaxed);
        });
        Self { pid, stop, done, handle: Some(handle) }
    }
}

impl Drop for Loopback {
    fn drop(&mut self) {
        self.stop.store(true, Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn run_loopback(pid: u32, rate: u32, src: &Arc<Source>, stop: &Arc<AtomicBool>) -> Result<(), Box<dyn Error>> {
    use wasapi::{AudioClient, Direction, SampleType, StreamMode, WaveFormat};

    let mut client = AudioClient::new_application_loopback_client(pid, true)?;
    let format = WaveFormat::new(32, 32, &SampleType::Float, rate as usize, 2, None);
    let mode = StreamMode::EventsShared { autoconvert: true, buffer_duration_hns: 0 };
    client.initialize_client(&format, &Direction::Capture, &mode)?;
    let event = client.set_get_eventhandle()?;
    let capture = client.get_audiocaptureclient()?;
    client.start_stream()?;
    *src.status.lock().unwrap() = format!("захват (PID {pid})");

    let mut raw: VecDeque<u8> = VecDeque::new();
    let mut samples: Vec<f32> = Vec::new();
    while !stop.load(Relaxed) {
        if event.wait_for_event(100).is_err() {
            continue; // таймаут: процесс сейчас ничего не играет
        }
        capture.read_from_device_to_deque(&mut raw)?;
        samples.clear();
        while raw.len() >= 8 {
            for _ in 0..2 {
                let b = [
                    raw.pop_front().unwrap_or(0),
                    raw.pop_front().unwrap_or(0),
                    raw.pop_front().unwrap_or(0),
                    raw.pop_front().unwrap_or(0),
                ];
                samples.push(f32::from_le_bytes(b));
            }
        }
        if !samples.is_empty() {
            src.push(&samples);
        }
    }
    let _ = client.stop_stream();
    Ok(())
}

/// Корневой процесс с заданным именем (родитель которого — не такой же exe).
fn find_root(sys: &System, exe: &str) -> Option<u32> {
    let matched: Vec<_> = sys
        .processes()
        .iter()
        .filter(|(_, p)| p.name().to_string_lossy().eq_ignore_ascii_case(exe))
        .collect();
    let ids: HashSet<Pid> = matched.iter().map(|(id, _)| **id).collect();
    matched
        .iter()
        .filter(|(_, p)| p.parent().map_or(true, |pp| !ids.contains(&pp)))
        .min_by_key(|(_, p)| p.start_time())
        .map(|(id, _)| id.as_u32())
}

// ───────────────────────────── cpal: вывод и микрофон ─────────────────────────────

fn find_dev<I: Iterator<Item = cpal::Device>>(it: Option<I>, pat: &str) -> Option<(cpal::Device, String)> {
    let p = pat.trim().to_lowercase();
    if p.is_empty() {
        return None;
    }
    for d in it? {
        if let Ok(n) = d.name() {
            if n.to_lowercase().contains(&p) {
                return Some((d, n));
            }
        }
    }
    None
}

/// Мягкий ограничитель: при большом усилении пики сжимаются плавно, а не режутся жёстко.
fn soft(x: f32) -> f32 {
    const K: f32 = 0.8;
    let a = x.abs();
    if a <= K {
        x
    } else {
        x.signum() * (K + (1.0 - K) * ((a - K) / (1.0 - K)).tanh())
    }
}

fn write_frame<T: SizedSample + FromSample<f32>>(frame: &mut [T], l: f32, r: f32) {
    if frame.len() == 1 {
        frame[0] = T::from_sample((l + r) * 0.5);
    } else {
        frame[0] = T::from_sample(l);
        frame[1] = T::from_sample(r);
        for x in frame[2..].iter_mut() {
            *x = T::from_sample(0.0f32);
        }
    }
}

fn build_out<T>(
    dev: &cpal::Device,
    cfg: &StreamConfig,
    list: Arc<Mutex<Vec<Arc<Source>>>>,
    shared: Arc<Shared>,
    err: Arc<AtomicBool>,
    mon: Arc<Monitor>,
) -> Result<Stream, cpal::BuildStreamError>
where
    T: SizedSample + FromSample<f32>,
{
    let ch = cfg.channels as usize;
    let rate = cfg.sample_rate.0;
    let mut mix: Vec<f32> = Vec::new();
    let mut mon_in: Vec<f32> = Vec::new();
    let mut mon_out: Vec<f32> = Vec::new();
    let mut rs = Resampler::new(rate, rate);
    let mut rs_rate = 0u32;
    dev.build_output_stream(
        cfg,
        move |data: &mut [T], _| {
            let frames = data.len() / ch;
            mix.clear();
            mix.resize(frames * 2, 0.0);
            {
                let l = list.lock().unwrap();
                for s in l.iter() {
                    s.mix_into(&mut mix, rate);
                }
            }
            let monitoring = mon.active.load(Relaxed);
            mon_in.clear();
            let mut pk = 0.0f32;
            for (i, frame) in data.chunks_mut(ch).enumerate() {
                let l = soft(mix[2 * i]);
                let r = soft(mix[2 * i + 1]);
                pk = pk.max(l.abs()).max(r.abs());
                write_frame(frame, l, r);
                if monitoring {
                    mon_in.push(l);
                    mon_in.push(r);
                }
            }
            if monitoring {
                let mr = mon.rate.load(Relaxed);
                if mr != 0 {
                    if mr != rs_rate {
                        rs = Resampler::new(rate, mr);
                        rs_rate = mr;
                    }
                    mon_out.clear();
                    rs.process(&mon_in, &mut mon_out);
                    mon.push(&mon_out);
                }
            }
            let old = f32::from_bits(shared.out_peak.load(Relaxed));
            shared.out_peak.store(pk.max(old * 0.9).to_bits(), Relaxed);
        },
        move |_e| err.store(true, Relaxed),
        None,
    )
}

/// Поток прослушивания: проигрывает копию итогового микса в выбранном устройстве.
fn build_mon<T>(
    dev: &cpal::Device,
    cfg: &StreamConfig,
    mon: Arc<Monitor>,
    err: Arc<AtomicBool>,
) -> Result<Stream, cpal::BuildStreamError>
where
    T: SizedSample + FromSample<f32>,
{
    let ch = cfg.channels as usize;
    let rate = cfg.sample_rate.0 as usize;
    let target = rate * 30 / 1000 * 2;
    let max = rate * 120 / 1000 * 2;
    dev.build_output_stream(
        cfg,
        move |data: &mut [T], _| {
            let vol = f32::from_bits(mon.volume.load(Relaxed));
            let mut b = mon.buf.lock().unwrap();
            let mut primed = mon.primed.load(Relaxed);
            if !primed && b.len() >= target {
                primed = true;
            }
            if primed && b.len() > max {
                let mut drop = b.len() - target;
                drop -= drop % 2;
                b.drain(..drop);
            }
            for frame in data.chunks_mut(ch) {
                let (l, r) = if primed && b.len() >= 2 {
                    let l = b.pop_front().unwrap_or(0.0) * vol;
                    let r = b.pop_front().unwrap_or(0.0) * vol;
                    (soft(l), soft(r))
                } else {
                    primed = false;
                    (0.0, 0.0)
                };
                write_frame(frame, l, r);
            }
            mon.primed.store(primed, Relaxed);
        },
        move |_e| err.store(true, Relaxed),
        None,
    )
}

const DN_RATE: u32 = 48_000;
const DN_FRAME: usize = 480; // 10 мс при 48 кГц — размер кадра RNNoise
const GATE_ATTACK: f32 = 0.007; // быстрое открытие гейта (~3 мс)
const GATE_RELEASE: f32 = 0.0004; // плавное закрытие (~50 мс)

fn build_in<T>(
    dev: &cpal::Device,
    cfg: &StreamConfig,
    out_rate: u32,
    src: Arc<Source>,
    err: Arc<AtomicBool>,
) -> Result<Stream, cpal::BuildStreamError>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let ch = cfg.channels as usize;
    // микрофон → 48 кГц → (шумоподавление) → частота устройства вывода
    let mut rs_in = Resampler::new(cfg.sample_rate.0, DN_RATE);
    let mut rs_out = Resampler::new(DN_RATE, out_rate);
    let mut dn = nnnoiseless::DenoiseState::new();
    let mut stereo: Vec<f32> = Vec::new();
    let mut s48: Vec<f32> = Vec::new();
    let mut proc: Vec<f32> = Vec::new();
    let mut out: Vec<f32> = Vec::new();
    let mut pend: Vec<f32> = Vec::new(); // моно, 48 кГц, в шкале i16
    let mut frame_out = [0.0f32; DN_FRAME];
    let mut gain = 1.0f32;
    let mut hold_left = 0i32;
    dev.build_input_stream(
        cfg,
        move |data: &[T], _| {
            stereo.clear();
            for fr in data.chunks(ch) {
                let l = f32::from_sample(fr[0]);
                let r = if ch > 1 { f32::from_sample(fr[1]) } else { l };
                stereo.push(l);
                stereo.push(r);
            }
            s48.clear();
            rs_in.process(&stereo, &mut s48);

            proc.clear();
            if src.denoise_on.load(Relaxed) {
                let amt = f32::from_bits(src.denoise_amt.load(Relaxed)).clamp(0.0, 1.0);
                for fr in s48.chunks_exact(2) {
                    pend.push((fr[0] + fr[1]) * 0.5 * 32768.0);
                }
                let gate_on = src.gate_on.load(Relaxed);
                let thr = f32::from_bits(src.gate_thr.load(Relaxed));
                let hold_frames = (f32::from_bits(src.gate_hold.load(Relaxed)) / 10.0).round() as i32;
                let floor = 10f32.powf(-f32::from_bits(src.gate_depth.load(Relaxed)) / 20.0);
                while pend.len() >= DN_FRAME {
                    let vad = dn.process_frame(&mut frame_out, &pend[..DN_FRAME]);
                    src.vad.store(vad.to_bits(), Relaxed);
                    let target = if gate_on {
                        if vad >= thr {
                            hold_left = hold_frames;
                        } else if hold_left > 0 {
                            hold_left -= 1;
                        }
                        if vad >= thr || hold_left > 0 { 1.0 } else { floor }
                    } else {
                        1.0
                    };
                    for i in 0..DN_FRAME {
                        let coef = if target > gain { GATE_ATTACK } else { GATE_RELEASE };
                        gain += (target - gain) * coef;
                        let dry = pend[i] / 32768.0;
                        let wet = frame_out[i] / 32768.0;
                        let v = (dry * (1.0 - amt) + wet * amt) * gain;
                        proc.push(v);
                        proc.push(v);
                    }
                    pend.drain(..DN_FRAME);
                }
            } else {
                pend.clear();
                src.vad.store(0, Relaxed);
                proc.extend_from_slice(&s48);
            }

            out.clear();
            rs_out.process(&proc, &mut out);
            src.push(&out);
        },
        move |_e| err.store(true, Relaxed),
        None,
    )
}

struct OutRt {
    _stream: Stream,
    name: String,
    rate: u32,
}

struct MicRt {
    _stream: Stream,
    name: String,
}

struct AppRt {
    exe: String,
    src: Arc<Source>,
    lb: Option<Loopback>,
    retry_after: Instant,
}

// ───────────────────────────── Движок ─────────────────────────────

struct Engine {
    cfg: Config,
    shared: Arc<Shared>,
    mix_list: Arc<Mutex<Vec<Arc<Source>>>>,
    out: Option<OutRt>,
    mic: Option<MicRt>,
    apps: Vec<AppRt>,
    out_err: Arc<AtomicBool>,
    mic_err: Arc<AtomicBool>,
    sys: System,
    mon: Arc<Monitor>,
    mon_out: Option<OutRt>,
    mon_err: Arc<AtomicBool>,
}

pub fn spawn(shared: Arc<Shared>, cfg: Config) -> Sender<Config> {
    let (tx, rx) = mpsc::channel::<Config>();
    std::thread::Builder::new()
        .name("engine".into())
        .spawn(move || {
            let mut e = Engine::new(shared, cfg);
            e.tick();
            loop {
                match rx.recv_timeout(Duration::from_millis(1000)) {
                    Ok(mut c) => {
                        while let Ok(n) = rx.try_recv() {
                            c = n;
                        }
                        e.apply(c);
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break,
                }
                e.tick();
            }
        })
        .expect("не удалось запустить поток движка");
    tx
}

impl Engine {
    fn new(shared: Arc<Shared>, cfg: Config) -> Self {
        let mut e = Self {
            cfg: cfg.clone(),
            shared,
            mix_list: Arc::new(Mutex::new(Vec::new())),
            out: None,
            mic: None,
            apps: Vec::new(),
            out_err: Arc::new(AtomicBool::new(false)),
            mic_err: Arc::new(AtomicBool::new(false)),
            sys: System::new(),
            mon: Monitor::new(),
            mon_out: None,
            mon_err: Arc::new(AtomicBool::new(false)),
        };
        e.apply(cfg);
        e
    }

    fn apply(&mut self, cfg: Config) {
        self.shared.mic.set(cfg.mic_volume, cfg.mic_delay_ms, true);
        self.shared.mic.denoise_on.store(cfg.denoise_on, Relaxed);
        self.shared.mic.denoise_amt.store(cfg.denoise_amount.to_bits(), Relaxed);
        self.shared.mic.gate_on.store(cfg.gate_on, Relaxed);
        self.shared.mic.gate_thr.store(cfg.gate_threshold.to_bits(), Relaxed);
        self.shared.mic.gate_hold.store(cfg.gate_hold_ms.to_bits(), Relaxed);
        self.shared.mic.gate_depth.store(cfg.gate_depth_db.to_bits(), Relaxed);

        self.apps.retain(|a| cfg.apps.iter().any(|c| c.exe.eq_ignore_ascii_case(&a.exe)));
        for c in &cfg.apps {
            if let Some(rt) = self.apps.iter_mut().find(|a| a.exe.eq_ignore_ascii_case(&c.exe)) {
                rt.src.set(c.volume, c.delay_ms, c.enabled);
            } else {
                let src = Source::new();
                src.set(c.volume, c.delay_ms, c.enabled);
                self.apps.push(AppRt { exe: c.exe.clone(), src, lb: None, retry_after: Instant::now() });
            }
        }

        let ordered: Vec<(String, Arc<Source>)> = cfg
            .apps
            .iter()
            .filter_map(|c| {
                self.apps
                    .iter()
                    .find(|a| a.exe.eq_ignore_ascii_case(&c.exe))
                    .map(|a| (c.exe.clone(), a.src.clone()))
            })
            .collect();
        let mut list = vec![self.shared.mic.clone()];
        list.extend(ordered.iter().map(|(_, s)| s.clone()));
        *self.mix_list.lock().unwrap() = list;
        *self.shared.apps.lock().unwrap() = ordered;

        self.cfg = cfg;
    }

    fn drop_inputs(&mut self) {
        self.mic = None;
        for a in self.apps.iter_mut() {
            a.lb = None;
        }
    }

    fn tick(&mut self) {
        let host = cpal::default_host();
        {
            let ins: Vec<String> = host
                .input_devices()
                .map(|it| it.filter_map(|d| d.name().ok()).collect())
                .unwrap_or_default();
            let outs: Vec<String> = host
                .output_devices()
                .map(|it| it.filter_map(|d| d.name().ok()).collect())
                .unwrap_or_default();
            let mut info = self.shared.info.lock().unwrap();
            info.in_devices = ins;
            info.out_devices = outs;
        }

        if !self.cfg.running {
            self.drop_inputs();
            self.out = None;
            self.stop_monitor();
            let mut info = self.shared.info.lock().unwrap();
            info.out_status = "Остановлено".into();
            info.monitor_status.clear();
            info.active_mic.clear();
            return;
        }

        self.ensure_output(&host);
        let Some(rate) = self.out.as_ref().map(|o| o.rate) else {
            self.stop_monitor();
            return;
        };
        self.ensure_mic(&host, rate);
        self.ensure_apps(rate);
        self.ensure_monitor(&host);
    }


    fn set_mon_status(&self, s: String) {
        self.shared.info.lock().unwrap().monitor_status = s;
    }

    fn stop_monitor(&mut self) {
        self.mon.active.store(false, Relaxed);
        self.mon.rate.store(0, Relaxed);
        self.mon.primed.store(false, Relaxed);
        self.mon.buf.lock().unwrap().clear();
        self.mon_out = None;
    }

    fn ensure_monitor(&mut self, host: &cpal::Host) {
        if self.mon_err.swap(false, Relaxed) {
            self.stop_monitor();
        }
        if !self.cfg.monitor_on {
            self.stop_monitor();
            self.set_mon_status(String::new());
            return;
        }
        self.mon.volume.store(self.cfg.monitor_volume.to_bits(), Relaxed);
        if self.cfg.monitor_device.trim().is_empty() {
            self.stop_monitor();
            self.set_mon_status("Выберите устройство для прослушивания".into());
            return;
        }
        let Some((dev, name)) = find_dev(host.output_devices().ok(), &self.cfg.monitor_device) else {
            self.stop_monitor();
            self.set_mon_status(format!("Устройство «{}» не найдено", self.cfg.monitor_device));
            return;
        };
        if self.out.as_ref().map(|o| o.name == name).unwrap_or(false) {
            self.stop_monitor();
            self.set_mon_status("Прослушивание совпадает с выходом — выберите наушники".into());
            return;
        }
        let same = self.mon_out.as_ref().map(|o| o.name == name).unwrap_or(false);
        if !same {
            self.stop_monitor();
            match self.build_monitor(&dev, &name) {
                Ok(o) => {
                    self.mon.rate.store(o.rate, Relaxed);
                    self.mon.active.store(true, Relaxed);
                    self.set_mon_status(format!("Слушаю: {} · {} Гц", o.name, o.rate));
                    self.mon_out = Some(o);
                }
                Err(e) => self.set_mon_status(format!("Ошибка прослушивания: {e}")),
            }
        }
    }

    fn build_monitor(&self, dev: &cpal::Device, name: &str) -> Result<OutRt, String> {
        let sup = dev.default_output_config().map_err(|e| e.to_string())?;
        let sc: StreamConfig = sup.config();
        let (m, f) = (self.mon.clone(), self.mon_err.clone());
        let stream = match sup.sample_format() {
            SampleFormat::F32 => build_mon::<f32>(dev, &sc, m, f),
            SampleFormat::I16 => build_mon::<i16>(dev, &sc, m, f),
            SampleFormat::I32 => build_mon::<i32>(dev, &sc, m, f),
            SampleFormat::U16 => build_mon::<u16>(dev, &sc, m, f),
            other => return Err(format!("формат {other:?} не поддерживается")),
        }
        .map_err(|e| e.to_string())?;
        stream.play().map_err(|e| e.to_string())?;
        Ok(OutRt { _stream: stream, name: name.to_string(), rate: sc.sample_rate.0 })
    }

    fn set_out_status(&self, s: String) {
        self.shared.info.lock().unwrap().out_status = s;
    }

    fn ensure_output(&mut self, host: &cpal::Host) {
        if self.out_err.swap(false, Relaxed) {
            self.drop_inputs();
            self.out = None;
        }
        let want = find_dev(host.output_devices().ok(), &self.cfg.output);
        match want {
            None => {
                if self.out.is_some() {
                    self.drop_inputs();
                    self.out = None;
                }
                self.set_out_status(format!("Устройство вывода «{}» не найдено", self.cfg.output));
            }
            Some((dev, name)) => {
                let same = self.out.as_ref().map(|o| o.name == name).unwrap_or(false);
                if !same {
                    self.drop_inputs();
                    self.out = None;
                    match self.build_output(&dev, &name) {
                        Ok(o) => {
                            self.set_out_status(format!("Вывод: {} · {} Гц", o.name, o.rate));
                            self.out = Some(o);
                        }
                        Err(e) => self.set_out_status(format!("Ошибка вывода: {e}")),
                    }
                }
            }
        }
    }

    fn build_output(&self, dev: &cpal::Device, name: &str) -> Result<OutRt, String> {
        let sup = dev.default_output_config().map_err(|e| e.to_string())?;
        let sc: StreamConfig = sup.config();
        let (l, s, f, m) = (self.mix_list.clone(), self.shared.clone(), self.out_err.clone(), self.mon.clone());
        let stream = match sup.sample_format() {
            SampleFormat::F32 => build_out::<f32>(dev, &sc, l, s, f, m),
            SampleFormat::I16 => build_out::<i16>(dev, &sc, l, s, f, m),
            SampleFormat::I32 => build_out::<i32>(dev, &sc, l, s, f, m),
            SampleFormat::U16 => build_out::<u16>(dev, &sc, l, s, f, m),
            other => return Err(format!("формат {other:?} не поддерживается")),
        }
        .map_err(|e| e.to_string())?;
        stream.play().map_err(|e| e.to_string())?;
        Ok(OutRt { _stream: stream, name: name.to_string(), rate: sc.sample_rate.0 })
    }

    fn ensure_mic(&mut self, host: &cpal::Host, out_rate: u32) {
        if self.mic_err.swap(false, Relaxed) {
            self.mic = None;
        }
        let mut want = None;
        for pat in &self.cfg.mics {
            if let Some(x) = find_dev(host.input_devices().ok(), pat) {
                want = Some(x);
                break;
            }
        }
        // Список пуст — берём микрофон Windows по умолчанию (кроме кабеля вывода, чтобы не зациклить звук)
        if want.is_none() && self.cfg.mics.iter().all(|m| m.trim().is_empty()) {
            if let Some(d) = host.default_input_device() {
                if let Ok(n) = d.name() {
                    let out = self.cfg.output.trim().to_lowercase();
                    if out.is_empty() || !n.to_lowercase().contains(&out) {
                        want = Some((d, n));
                    }
                }
            }
        }
        match want {
            None => {
                self.mic = None;
                *self.shared.mic.status.lock().unwrap() = "микрофон не найден".into();
                self.shared.info.lock().unwrap().active_mic.clear();
            }
            Some((dev, name)) => {
                let same = self.mic.as_ref().map(|m| m.name == name).unwrap_or(false);
                if !same {
                    self.mic = None;
                    match start_mic(&dev, &name, out_rate, self.shared.mic.clone(), self.mic_err.clone()) {
                        Ok(m) => {
                            *self.shared.mic.status.lock().unwrap() = format!("активен: {}", m.name);
                            self.shared.info.lock().unwrap().active_mic = m.name.clone();
                            self.mic = Some(m);
                        }
                        Err(e) => {
                            *self.shared.mic.status.lock().unwrap() = format!("ошибка: {e}");
                            self.shared.info.lock().unwrap().active_mic.clear();
                        }
                    }
                }
            }
        }
    }

    fn ensure_apps(&mut self, rate: u32) {
        self.sys.refresh_processes(ProcessesToUpdate::All, true);
        for a in self.apps.iter_mut() {
            let enabled = self
                .cfg
                .apps
                .iter()
                .find(|c| c.exe.eq_ignore_ascii_case(&a.exe))
                .map(|c| c.enabled)
                .unwrap_or(false);

            if let Some(lb) = &a.lb {
                let gone = self.sys.process(Pid::from_u32(lb.pid)).is_none();
                if lb.done.load(Relaxed) || gone || !enabled {
                    a.lb = None;
                }
            }
            if !enabled {
                *a.src.status.lock().unwrap() = "выключено".into();
                continue;
            }
            if a.lb.is_none() {
                match find_root(&self.sys, &a.exe) {
                    None => *a.src.status.lock().unwrap() = "ждёт запуска".into(),
                    Some(pid) => {
                        if Instant::now() >= a.retry_after {
                            *a.src.status.lock().unwrap() = format!("запуск захвата (PID {pid})");
                            a.lb = Some(Loopback::start(pid, rate, a.src.clone()));
                            a.retry_after = Instant::now() + Duration::from_secs(3);
                        }
                    }
                }
            }
        }
    }
}

fn start_mic(
    dev: &cpal::Device,
    name: &str,
    out_rate: u32,
    src: Arc<Source>,
    err: Arc<AtomicBool>,
) -> Result<MicRt, String> {
    let sup = dev.default_input_config().map_err(|e| e.to_string())?;
    let sc: StreamConfig = sup.config();
    let stream = match sup.sample_format() {
        SampleFormat::F32 => build_in::<f32>(dev, &sc, out_rate, src, err),
        SampleFormat::I16 => build_in::<i16>(dev, &sc, out_rate, src, err),
        SampleFormat::I32 => build_in::<i32>(dev, &sc, out_rate, src, err),
        SampleFormat::U16 => build_in::<u16>(dev, &sc, out_rate, src, err),
        other => return Err(format!("формат {other:?} не поддерживается")),
    }
    .map_err(|e| e.to_string())?;
    stream.play().map_err(|e| e.to_string())?;
    Ok(MicRt { _stream: stream, name: name.to_string() })
}
