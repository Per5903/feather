//! Голос: свой аудиотракт вокруг LiveKit (docs/06-voice.md, решение D17).
//!
//! ```text
//! микрофон → cpal → [кольцо] → APM (эхо, автогромкость, фильтр) → RNNoise → LiveKit
//! LiveKit → по кольцу на собеседника → микшер в колбэке вывода → cpal → динамики
//!                                                   └→ [кольцо] → APM (опорный сигнал для эхоподавления)
//! ```
//!
//! Пока микрофон выключен, кадры в кодировщик не подаются вовсе: кодировщик простаивает
//! (bench/README.md, замер 4 — у Discord он работает даже при выключенном микрофоне).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use futures_util::StreamExt;
use livekit::options::TrackPublishOptions;
use livekit::prelude::*;
use livekit::webrtc::audio_frame::AudioFrame;
use livekit::webrtc::audio_source::AudioSourceOptions;
use livekit::webrtc::audio_source::native::NativeAudioSource;
use livekit::webrtc::audio_stream::native::NativeAudioStream;
use livekit::webrtc::native::apm::AudioProcessingModule;
use nnnoiseless::DenoiseState;
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};

const SAMPLE_RATE: u32 = 48_000;
/// 10 мс при 48 кГц: размер кадра и для APM, и для RNNoise, и для LiveKit
const FRAME: usize = 480;
/// Запас колец: 200 мс. Больше — значит, обработка не успевает, лишнее выбрасываем
const RING: usize = FRAME * 20;
/// Порог вероятности речи RNNoise для активации голосом
const VAD_THRESHOLD: f32 = 0.6;
/// Без шумодава: порог громкости (≈ −40 dBFS)
const ENERGY_GATE_RMS: f64 = 330.0;
/// После речи держим передачу ещё 400 мс: не обрезаем окончания слов и короткие паузы
const GATE_HANGOVER_FRAMES: u32 = 40;
/// Линейный пересчёт частоты моно-потока: для устройств не на 48 кГц
/// (на Windows WASAPI в общем режиме часто 44,1 кГц). Для голоса качества хватает.
pub struct Resampler {
    /// Сколько входных сэмплов на один выходной
    step: f64,
    /// Позиция следующего выходного сэмпла относительно начала очередного куска; −1 — `prev`
    pos: f64,
    prev: f32,
}

impl Resampler {
    pub fn new(from: u32, to: u32) -> Self {
        Self { step: from as f64 / to as f64, pos: 0.0, prev: 0.0 }
    }

    pub fn process(&mut self, input: &[f32], mut emit: impl FnMut(f32)) {
        if input.is_empty() {
            return;
        }
        let n = input.len() as f64;
        while self.pos < n - 1.0 {
            let i = self.pos.floor();
            let frac = (self.pos - i) as f32;
            let a = if i < 0.0 { self.prev } else { input[i as usize] };
            let b = input[(i + 1.0) as usize];
            emit(a + (b - a) * frac);
            self.pos += self.step;
        }
        self.pos -= n;
        self.prev = input[input.len() - 1];
    }
}

/// Размер буфера устройства оставляем системе. Замер 11: фиксированный буфер 20 мс в бэкенде
/// PulseAudio поднял нагрузку аудиопотока с 3,4 до 8,7 % — хуже, а не лучше.
fn with_buffer<S>(mut build: impl FnMut(cpal::BufferSize) -> Option<S>) -> Option<S> {
    build(cpal::BufferSize::Default)
}

#[derive(Clone, Debug)]
pub struct VoicePeer {
    pub identity: String,
    pub name: String,
}

impl VoicePeer {
    /// Имя выдаёт сервер Feather в токене; если его нет (сторонний клиент) — показываем identity
    fn new(identity: String, name: String) -> Self {
        let name = if name.trim().is_empty() { identity.clone() } else { name };
        Self { identity, name }
    }
}

#[derive(Debug)]
pub enum VoiceEvent {
    /// Подключились: кто уже в канале (без нас)
    Joined { me: String, peers: Vec<VoicePeer> },
    PeerJoined(VoicePeer),
    PeerLeft(String),
    /// Кто сейчас говорит (identity, включая нас)
    Speaking(Vec<String>),
    /// Некритичная проблема: например, нет микрофона — слушать можно, говорить нет
    Warning(String),
    Disconnected(String),
}

#[derive(Clone, Debug)]
pub struct VoiceOptions {
    /// Шумодав RNNoise
    pub noise_suppression: bool,
    /// Эхоподавление (нужно с колонками; в наушниках можно выключить и сэкономить CPU)
    pub echo_cancellation: bool,
    /// Микрофон по подстроке имени; None — системный по умолчанию.
    /// Если указан и не найден — работаем без микрофона, а не берём другой.
    pub input_device: Option<String>,
    /// Устройство вывода по подстроке имени; None — по умолчанию
    pub output_device: Option<String>,
    /// Активация голосом: передавать звук, только когда в нём есть речь (VAD RNNoise).
    /// В паузах кодировщик простаивает (экономия CPU и трафика), клавиатуру между фразами не слышно.
    /// Для push-to-talk не нужна: там решает клавиша.
    pub voice_activation: bool,
}

impl Default for VoiceOptions {
    fn default() -> Self {
        Self { noise_suppression: true, echo_cancellation: true, input_device: None, output_device: None, voice_activation: true }
    }
}

/// Список устройств ввода и вывода (для настроек клиента)
pub fn list_devices() -> (Vec<String>, Vec<String>) {
    let host = cpal::default_host();
    let names = |it: Option<Vec<cpal::Device>>| -> Vec<String> {
        it.unwrap_or_default().iter().filter_map(|d| d.description().ok().map(|x| x.name().to_owned())).collect()
    };
    (
        names(host.input_devices().ok().map(|d| d.collect())),
        names(host.output_devices().ok().map(|d| d.collect())),
    )
}

fn find_device(devices: Option<impl Iterator<Item = cpal::Device>>, want: &str) -> Option<cpal::Device> {
    let want = want.to_lowercase();
    devices?.find(|d| d.description().is_ok_and(|x| x.name().to_lowercase().contains(&want)))
}

type Mixer = Mutex<Vec<(String, HeapCons<f32>)>>;

struct Shared {
    muted: AtomicBool,
    deafened: AtomicBool,
    stop: AtomicBool,
    /// Кольца собеседников: identity → поток сэмплов
    mixer: Mixer,
}

pub struct VoiceSession {
    room: Arc<Room>,
    shared: Arc<Shared>,
    audio_thread: Option<std::thread::JoinHandle<()>>,
}

impl VoiceSession {
    /// Подключение к комнате LiveKit и запуск аудиоустройств
    pub async fn join(
        url: &str,
        token: &str,
        opts: VoiceOptions,
        on_event: impl Fn(VoiceEvent) + Send + Sync + 'static,
    ) -> Result<Self, String> {
        let on_event = Arc::new(on_event);
        let (room, mut events) = Room::connect(url, token, RoomOptions::default())
            .await
            .map_err(|e| format!("не удалось подключиться к голосовому серверу: {e}"))?;
        let room = Arc::new(room);
        let shared = Arc::new(Shared {
            muted: AtomicBool::new(false),
            deafened: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            mixer: Mutex::new(Vec::new()),
        });

        // Микрофон: источник с выключенной встроенной обработкой — обрабатываем сами
        let source = NativeAudioSource::new(AudioSourceOptions::default(), SAMPLE_RATE, 1, 0);
        let track = LocalAudioTrack::create_audio_track("mic", RtcAudioSource::Native(source.clone()));
        room.local_participant()
            .publish_track(
                LocalTrack::Audio(track),
                TrackPublishOptions { source: TrackSource::Microphone, dtx: true, ..Default::default() },
            )
            .await
            .map_err(|e| format!("не удалось включить микрофон: {e}"))?;

        // Аудиоустройства — в отдельном потоке: потоки cpal нельзя переносить между потоками
        let (cap_prod, cap_cons) = HeapRb::<f32>::new(RING).split();
        let (rev_prod, rev_cons) = HeapRb::<f32>::new(RING).split();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let audio_thread = {
            let shared = shared.clone();
            let (inp, out) = (opts.input_device.clone(), opts.output_device.clone());
            std::thread::Builder::new()
                .name("feather-audio".into())
                .spawn(move || audio_devices(shared, inp, out, cap_prod, rev_prod, ready_tx))
                .map_err(|e| e.to_string())?
        };
        for warning in ready_rx.recv_timeout(Duration::from_secs(5)).unwrap_or_default() {
            on_event(VoiceEvent::Warning(warning));
        }

        tokio::spawn(process_capture(shared.clone(), source, cap_cons, rev_cons, opts.clone()));

        let me = room.local_participant().identity().to_string();
        let peers = room
            .remote_participants()
            .values()
            .map(|p| VoicePeer::new(p.identity().to_string(), p.name().to_string()))
            .collect();
        on_event(VoiceEvent::Joined { me, peers });

        // События комнаты
        {
            let shared = shared.clone();
            tokio::spawn(async move {
                while let Some(ev) = events.recv().await {
                    match ev {
                        RoomEvent::TrackSubscribed { track: RemoteTrack::Audio(t), participant, .. } => {
                            play_remote(shared.clone(), participant.identity().to_string(), t);
                        }
                        RoomEvent::ParticipantConnected(p) => {
                            on_event(VoiceEvent::PeerJoined(VoicePeer::new(p.identity().to_string(), p.name().to_string())))
                        }
                        RoomEvent::ParticipantDisconnected(p) => {
                            let id = p.identity().to_string();
                            shared.mixer.lock().unwrap().retain(|(who, _)| *who != id);
                            on_event(VoiceEvent::PeerLeft(id));
                        }
                        RoomEvent::ActiveSpeakersChanged { speakers } => {
                            on_event(VoiceEvent::Speaking(speakers.iter().map(|p| p.identity().to_string()).collect()));
                        }
                        RoomEvent::Disconnected { reason } => {
                            on_event(VoiceEvent::Disconnected(format!("{reason:?}")));
                            break;
                        }
                        _ => {}
                    }
                }
            });
        }

        Ok(Self { room, shared, audio_thread: Some(audio_thread) })
    }

    pub fn set_muted(&self, muted: bool) {
        self.shared.muted.store(muted, Ordering::Relaxed);
    }

    /// «Выключить звук»: не слышим других (и, как в Discord, сами тоже молчим)
    pub fn set_deafened(&self, deafened: bool) {
        self.shared.deafened.store(deafened, Ordering::Relaxed);
    }

    pub async fn leave(mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        let _ = self.room.close().await;
        if let Some(t) = self.audio_thread.take() {
            let _ = tokio::task::spawn_blocking(move || t.join()).await;
        }
    }
}

impl Drop for VoiceSession {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
    }
}

/// Поток аудиоустройств: открывает микрофон и вывод, живёт до `stop`
fn audio_devices(
    shared: Arc<Shared>,
    input_name: Option<String>,
    output_name: Option<String>,
    cap: HeapProd<f32>,
    rev: HeapProd<f32>,
    ready: std::sync::mpsc::Sender<Vec<String>>,
) {
    let host = cpal::default_host();
    let mut warnings = Vec::new();

    let input_dev = match &input_name {
        Some(want) => find_device(host.input_devices().ok(), want),
        None => host.default_input_device(),
    };
    let output_dev = match &output_name {
        Some(want) => find_device(host.output_devices().ok(), want),
        None => host.default_output_device(),
    };

    let cap = Arc::new(Mutex::new(cap));
    let input = input_dev.and_then(|dev| {
        let default = dev.default_input_config().ok()?;
        let channels = default.channels();
        let ch = channels as usize;
        // Сначала 48 кГц; не согласно устройство — его родная частота и пересчёт
        [SAMPLE_RATE, default.sample_rate()].into_iter().find_map(|rate| {
            with_buffer(|buffer_size| {
                let cap = cap.clone();
                let mut rs = (rate != SAMPLE_RATE).then(|| Resampler::new(rate, SAMPLE_RATE));
                let mut mono = Vec::new();
                dev.build_input_stream::<f32, _, _>(
                    cpal::StreamConfig { channels, sample_rate: rate, buffer_size },
                    move |data, _| {
                        // Колбэк один, блокировка никогда не ждёт; сводим в моно средним каналов
                        let Ok(mut cap) = cap.try_lock() else { return };
                        mono.clear();
                        mono.extend(data.chunks(ch).map(|f| f.iter().sum::<f32>() / ch as f32));
                        match rs.as_mut() {
                            Some(rs) => rs.process(&mono, |s| {
                                let _ = cap.try_push(s);
                            }),
                            None => {
                                let _ = cap.push_slice(&mono);
                            }
                        }
                    },
                    |e| eprintln!("микрофон: {e}"),
                    None,
                )
                .ok()
            })
        })
    });
    if input.is_none() {
        warnings.push("Микрофон не найден: вас не слышно, но вы слышите других".to_owned());
    }

    let rev = Arc::new(Mutex::new(rev));
    let output = output_dev.and_then(|dev| {
        let default = dev.default_output_config().ok()?;
        let channels = default.channels();
        let ch = channels as usize;
        [SAMPLE_RATE, default.sample_rate()].into_iter().find_map(|rate| {
            with_buffer(|buffer_size| {
                let shared = shared.clone();
                let rev = rev.clone();
                let mut rs = (rate != SAMPLE_RATE).then(|| Resampler::new(SAMPLE_RATE, rate));
                let mut queue = std::collections::VecDeque::<f32>::new();
                let mut mono = Vec::<f32>::new();
                let mut tmp = Vec::<f32>::new();
                // Смесь собеседников (48 кГц), она же опорный сигнал для эхоподавления
                let mut mix = move |n: usize, mono: &mut Vec<f32>| {
                    mono.clear();
                    mono.resize(n, 0.0);
                    // try_lock: аудиопоток никогда не ждёт; в редкий момент смены участников — тишина
                    if let Ok(mut mixer) = shared.mixer.try_lock() {
                        for (_, cons) in mixer.iter_mut() {
                            tmp.clear();
                            tmp.resize(n, 0.0);
                            let got = cons.pop_slice(&mut tmp);
                            for (m, s) in mono.iter_mut().zip(&tmp[..got]) {
                                *m += s;
                            }
                        }
                    }
                    if shared.deafened.load(Ordering::Relaxed) {
                        mono.iter_mut().for_each(|s| *s = 0.0);
                    }
                    if let Ok(mut rev) = rev.try_lock() {
                        let _ = rev.push_slice(mono);
                    }
                };
                dev.build_output_stream::<f32, _, _>(
                    cpal::StreamConfig { channels, sample_rate: rate, buffer_size },
                    move |out: &mut [f32], _| {
                        let frames = out.len() / ch;
                        match rs.as_mut() {
                            None => mix(frames, &mut mono),
                            Some(rs) => {
                                // Набираем 48 кГц кусками по 10 мс и пересчитываем в частоту устройства
                                while queue.len() < frames {
                                    mix(FRAME, &mut mono);
                                    rs.process(&mono, |s| queue.push_back(s));
                                }
                                mono.clear();
                                mono.extend(queue.drain(..frames));
                            }
                        }
                        for (frame, s) in out.chunks_mut(ch).zip(&mono) {
                            frame.fill(s.clamp(-1.0, 1.0));
                        }
                    },
                    |e| eprintln!("вывод звука: {e}"),
                    None,
                )
                .ok()
            })
        })
    });
    if output.is_none() {
        warnings.push("Устройство вывода звука не найдено".to_owned());
    }

    for s in input.iter().chain(output.iter()) {
        let _ = s.play();
    }
    let _ = ready.send(warnings);
    while !shared.stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(100));
    }
    // Потоки cpal закрываются при выходе из области видимости
}

/// Обработка микрофона кадрами по 10 мс: APM → RNNoise → LiveKit
async fn process_capture(
    shared: Arc<Shared>,
    source: NativeAudioSource,
    mut cap: HeapCons<f32>,
    mut rev: HeapCons<f32>,
    opts: VoiceOptions,
) {
    let VoiceOptions { noise_suppression, echo_cancellation, voice_activation, .. } = opts;
    let mut apm = AudioProcessingModule::new(echo_cancellation, true, true, false);
    // Активация голосом: открыта ещё столько кадров после последнего кадра с речью
    let mut gate_open_frames = 0u32;
    let mut denoise = DenoiseState::new();
    let mut frame = [0f32; FRAME];
    let mut pcm = [0i16; FRAME];
    let mut reverse = [0f32; FRAME];
    let mut reverse_pcm = [0i16; FRAME];
    let mut clean = [0f32; FRAME];
    // Раз в 10 мс (длина кадра); в буфере устройства 20 мс, поэтому за тик бывает 0–2 кадра
    let mut tick = tokio::time::interval(Duration::from_millis(10));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    while !shared.stop.load(Ordering::Relaxed) {
        tick.tick().await;
        while cap.occupied_len() >= FRAME {
            cap.pop_slice(&mut frame);
            let n = rev.pop_slice(&mut reverse);
            reverse[n..].fill(0.0);

            // Микрофон выключен (или PTT отпущен): кадр выбрасываем, кодировщик простаивает
            if shared.muted.load(Ordering::Relaxed) || shared.deafened.load(Ordering::Relaxed) {
                continue;
            }
            for (o, s) in pcm.iter_mut().zip(&frame) {
                *o = (s * 32767.0).clamp(-32768.0, 32767.0) as i16;
            }
            if echo_cancellation {
                for (o, s) in reverse_pcm.iter_mut().zip(&reverse) {
                    *o = (s * 32767.0).clamp(-32768.0, 32767.0) as i16;
                }
                let _ = apm.process_reverse_stream(&mut reverse_pcm, SAMPLE_RATE as i32, 1);
            }
            let _ = apm.process_stream(&mut pcm, SAMPLE_RATE as i32, 1);

            // Вероятность речи в кадре: от RNNoise, а без шумодава — по громкости
            let speech = if noise_suppression {
                // RNNoise работает в шкале i16, но во float
                for (o, s) in frame.iter_mut().zip(&pcm) {
                    *o = *s as f32;
                }
                let vad = denoise.process_frame(&mut clean, &frame);
                for (o, s) in pcm.iter_mut().zip(&clean) {
                    *o = s.clamp(-32768.0, 32767.0) as i16;
                }
                vad > VAD_THRESHOLD
            } else {
                let rms = (pcm.iter().map(|&s| (s as f64).powi(2)).sum::<f64>() / FRAME as f64).sqrt();
                rms > ENERGY_GATE_RMS
            };
            if voice_activation {
                if speech {
                    gate_open_frames = GATE_HANGOVER_FRAMES;
                } else if gate_open_frames == 0 {
                    continue; // тишина: кадр не кодируем и не отправляем
                } else {
                    gate_open_frames -= 1;
                }
            }
            let af = AudioFrame { data: pcm.as_slice().into(), sample_rate: SAMPLE_RATE, num_channels: 1, samples_per_channel: FRAME as u32 };
            if source.capture_frame(&af).await.is_err() {
                return;
            }
        }
    }
}

/// Звук собеседника: декодированные кадры LiveKit → его кольцо в микшере
fn play_remote(shared: Arc<Shared>, identity: String, track: RemoteAudioTrack) {
    let (mut prod, cons) = HeapRb::<f32>::new(RING).split();
    {
        let mut mixer = shared.mixer.lock().unwrap();
        mixer.retain(|(who, _)| *who != identity);
        mixer.push((identity, cons));
    }
    tokio::spawn(async move {
        let mut stream = NativeAudioStream::new(track.rtc_track(), SAMPLE_RATE as i32, 1);
        let mut buf = Vec::new();
        while let Some(frame) = stream.next().await {
            if shared.stop.load(Ordering::Relaxed) {
                break;
            }
            buf.clear();
            buf.extend(frame.data.iter().map(|s| *s as f32 / 32768.0));
            // Если вывод отстаёт (кольцо полно) — лишнее отбрасываем, а не копим задержку
            let _ = prod.push_slice(&buf);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::Resampler;

    #[test]
    fn resampler_rates_and_continuity() {
        // 44,1 → 48 кГц: за секунду столько же выходных сэмплов (±1), без разрывов между кусками
        let mut rs = Resampler::new(44_100, 48_000);
        let input: Vec<f32> = (0..44_100).map(|i| (i as f32 * 0.01).sin()).collect();
        let mut out = Vec::new();
        for chunk in input.chunks(441) {
            rs.process(chunk, |s| out.push(s));
        }
        assert!((out.len() as i64 - 48_000).abs() <= 2, "получилось {}", out.len());
        let max_jump = out.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0f32, f32::max);
        assert!(max_jump < 0.02, "скачок {max_jump}: разрыв на стыке кусков");
    }
}
