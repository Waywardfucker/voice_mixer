//! Soundpad: проигрывание звуков (mp3, wav, ogg, flac, m4a/aac) прямо в итоговый микс.
//! Файлы декодируются заранее в стерео на частоте устройства вывода и кэшируются в памяти.

use crate::engine::Resampler;
use std::collections::HashMap;
use std::fs::File;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

/// Максимальная длина одного звука (дальше обрезается), чтобы не занимать много памяти.
const MAX_SECONDS: usize = 120;
/// Предел памяти под кэш декодированных звуков (самые давно не игравшие выгружаются).
const CACHE_BUDGET_BYTES: usize = 192 * 1024 * 1024;
/// Декодирование по одному файлу за раз, чтобы не раздувать память пиками.
static DECODE_LOCK: Mutex<()> = Mutex::new(());
/// Максимум одновременно играющих звуков.
const MAX_VOICES: usize = 16;

pub const EXTENSIONS: [&str; 6] = ["mp3", "wav", "ogg", "flac", "m4a", "aac"];

struct Voice {
    data: Arc<Vec<f32>>,
    pos: usize,
    vol: f32,
    looped: bool,
    path: String,
}

struct CacheEntry {
    data: Arc<Vec<f32>>,
    used: u64,
}

pub struct Pad {
    voices: Mutex<Vec<Voice>>,
    cache: Mutex<HashMap<(String, u32), CacheEntry>>,
    clock: AtomicU64,
    /// Частота устройства вывода (0 — вывод не работает).
    pub rate: AtomicU32,
    master: AtomicU32,
    pub status: Mutex<String>,
}

impl Pad {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            voices: Mutex::new(Vec::new()),
            cache: Mutex::new(HashMap::new()),
            clock: AtomicU64::new(0),
            rate: AtomicU32::new(0),
            master: AtomicU32::new(1.0f32.to_bits()),
            status: Mutex::new(String::new()),
        })
    }

    pub fn set_master(&self, v: f32) {
        self.master.store(v.to_bits(), Relaxed);
    }

    pub fn playing(&self) -> usize {
        self.voices.lock().unwrap().len()
    }

    pub fn is_playing(&self, path: &str) -> bool {
        self.voices.lock().unwrap().iter().any(|v| v.path == path)
    }

    pub fn stop_all(&self) {
        self.voices.lock().unwrap().clear();
    }

    pub fn stop(&self, path: &str) {
        self.voices.lock().unwrap().retain(|v| v.path != path);
    }

    /// Сколько памяти занимает кэш декодированных звуков, в байтах.
    pub fn cache_bytes(&self) -> usize {
        self.cache.lock().unwrap().values().map(|e| e.data.len() * 4).sum()
    }

    /// Очистить кэш (кроме звуков, которые играют прямо сейчас).
    pub fn clear_cache(&self) {
        self.cache.lock().unwrap().retain(|_, e| Arc::strong_count(&e.data) > 1);
    }

    fn insert_cache(&self, key: (String, u32), data: Arc<Vec<f32>>) {
        let tick = self.clock.fetch_add(1, Relaxed);
        let mut c = self.cache.lock().unwrap();
        c.insert(key.clone(), CacheEntry { data, used: tick });
        let mut total: usize = c.values().map(|e| e.data.len() * 4).sum();
        while total > CACHE_BUDGET_BYTES {
            let victim = c
                .iter()
                .filter(|(k, e)| **k != key && Arc::strong_count(&e.data) == 1)
                .min_by_key(|(_, e)| e.used)
                .map(|(k, _)| k.clone());
            match victim {
                Some(k) => {
                    if let Some(e) = c.remove(&k) {
                        total -= e.data.len() * 4;
                    }
                }
                None => break,
            }
        }
    }

    /// Убрать из кэша звуки, которых больше нет в списке.
    pub fn retain_paths(&self, paths: &[String]) {
        self.cache.lock().unwrap().retain(|(p, _), _| paths.iter().any(|x| x == p));
    }

    /// Домешать играющие звуки в `out` (стерео interleaved на частоте вывода).
    pub fn mix_into(&self, out: &mut [f32]) {
        let master = f32::from_bits(self.master.load(Relaxed));
        let mut voices = self.voices.lock().unwrap();
        if voices.is_empty() {
            return;
        }
        for v in voices.iter_mut() {
            let len = v.data.len();
            let gain = v.vol * master;
            for o in out.iter_mut() {
                if v.pos >= len {
                    if v.looped && len > 0 {
                        v.pos = 0;
                    } else {
                        break;
                    }
                }
                *o += v.data[v.pos] * gain;
                v.pos += 1;
            }
        }
        voices.retain(|v| v.looped || v.pos < v.data.len());
    }

    fn start(&self, data: Arc<Vec<f32>>, path: &str, vol: f32, looped: bool) {
        if data.is_empty() {
            return;
        }
        let mut voices = self.voices.lock().unwrap();
        if voices.len() >= MAX_VOICES {
            voices.remove(0);
        }
        voices.push(Voice { data, pos: 0, vol, looped, path: path.to_string() });
    }

    /// Запустить звук. Зацикленный звук при повторном вызове останавливается.
    pub fn play(self: &Arc<Self>, path: &str, vol: f32, looped: bool) {
        if looped && self.is_playing(path) {
            self.stop(path);
            return;
        }
        let rate = self.rate.load(Relaxed);
        if rate == 0 {
            *self.status.lock().unwrap() = "Микшер остановлен: звуку некуда играть".into();
            return;
        }
        let cached = {
            let mut c = self.cache.lock().unwrap();
            c.get_mut(&(path.to_string(), rate)).map(|e| {
                e.used = self.clock.fetch_add(1, Relaxed);
                e.data.clone()
            })
        };
        if let Some(d) = cached {
            self.start(d, path, vol, looped);
            return;
        }
        let me = self.clone();
        let path = path.to_string();
        std::thread::spawn(move || {
            let _g = DECODE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            match decode_file(&path, rate) {
            Ok(d) => {
                let d = Arc::new(d);
                me.insert_cache((path.clone(), rate), d.clone());
                me.start(d, &path, vol, looped);
                me.status.lock().unwrap().clear();
            }
            Err(e) => {
                *me.status.lock().unwrap() = format!("{}: {e}", short_name(&path));
            }
            }
        });
    }

    /// Заранее декодировать файл в кэш (чтобы первое нажатие бинда не задерживалось).
    pub fn preload(self: &Arc<Self>, path: &str) {
        let rate = self.rate.load(Relaxed);
        if rate == 0 || self.cache.lock().unwrap().contains_key(&(path.to_string(), rate)) {
            return;
        }
        let me = self.clone();
        let path = path.to_string();
        std::thread::spawn(move || {
            let _g = DECODE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            match decode_file(&path, rate) {
                Ok(d) => me.insert_cache((path.clone(), rate), Arc::new(d)),
                Err(e) => {
                    *me.status.lock().unwrap() = format!("{}: {e}", short_name(&path));
                }
            }
        });
    }
}

fn short_name(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path)
        .to_string()
}

/// Декодировать файл в стерео interleaved f32 на частоте `out_rate`.
fn decode_file(path: &str, out_rate: u32) -> Result<Vec<f32>, String> {
    let file = File::open(path).map_err(|e| format!("не удалось открыть файл ({e})"))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = std::path::Path::new(path).extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
        .map_err(|e| format!("формат не поддерживается ({e})"))?;
    let mut format = probed.format;
    let (track_id, in_rate, params) = {
        let track = format
            .tracks()
            .iter()
            .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
            .ok_or("в файле нет звуковой дорожки")?;
        let rate = track.codec_params.sample_rate.ok_or("неизвестная частота дискретизации")?;
        (track.id, rate, track.codec_params.clone())
    };
    let mut decoder = symphonia::default::get_codecs()
        .make(&params, &DecoderOptions::default())
        .map_err(|e| format!("кодек не поддерживается ({e})"))?;

    let max = in_rate as usize * MAX_SECONDS * 2;
    let mut stereo: Vec<f32> = Vec::new();
    let mut sbuf: Option<SampleBuffer<f32>> = None;
    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymError::IoError(_)) | Err(SymError::ResetRequired) => break,
            Err(e) => return Err(format!("{e}")),
        };
        if packet.track_id() != track_id {
            continue;
        }
        match decoder.decode(&packet) {
            Ok(decoded) => {
                let spec = *decoded.spec();
                let ch = spec.channels.count().max(1);
                let need = decoded.capacity() * ch;
                if sbuf.as_ref().map(|b| b.capacity() < need).unwrap_or(true) {
                    sbuf = Some(SampleBuffer::<f32>::new(decoded.capacity() as u64, spec));
                }
                if let Some(b) = sbuf.as_mut() {
                    b.copy_interleaved_ref(decoded);
                    for fr in b.samples().chunks_exact(ch) {
                        let l = fr[0];
                        let r = if ch > 1 { fr[1] } else { fr[0] };
                        stereo.push(l);
                        stereo.push(r);
                    }
                }
                if stereo.len() >= max {
                    break;
                }
            }
            Err(SymError::DecodeError(_)) => continue,
            Err(_) => break,
        }
    }
    if stereo.is_empty() {
        return Err("файл пустой или не удалось декодировать".into());
    }
    let mut out: Vec<f32> = if in_rate == out_rate {
        stereo
    } else {
        let mut rs = Resampler::new(in_rate, out_rate);
        let mut o = Vec::with_capacity(stereo.len() * out_rate as usize / in_rate as usize + 8);
        rs.process(&stereo, &mut o);
        o
    };
    if out.len() % 2 != 0 {
        out.pop();
    }
    out.shrink_to_fit();
    Ok(out)
}
