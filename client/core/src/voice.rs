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
/// Сколько звука собеседника держим в очереди вывода сверх одного буфера устройства.
/// Сеть сглаживает джиттер-буфер WebRTC, нам запас почти не нужен: всё сверх — сбрасываем,
/// иначе после сетевого всплеска или из-за разницы часов звуковых карт задержка копится навсегда.
const MAX_QUEUED: usize = FRAME * 6;
const TRIM_TO: usize = FRAME * 3;
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
    /// Качество связи с голосовым сервером, раз в 2 секунды
    Stats(VoiceStats),
    /// Участник начал (true) или закончил (false) показ экрана
    Screen { identity: String, live: bool },
    /// Мой показ экрана прекратился сам (отменён в окне системы, окно закрыто)
    ScreenEnded(String),
    Disconnected(String),
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct VoiceStats {
    /// Пинг до голосового сервера, мс
    pub rtt_ms: Option<u32>,
    /// Как идёт звук: «UDP», «TCP» или «TURN» (через ретранслятор)
    pub transport: String,
    /// Потери входящих пакетов за последний интервал, %
    pub loss_pct: f32,
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
    /// Громкость собеседников по identity (id пользователя), 1.0 — как есть
    pub volumes: std::collections::HashMap<String, f32>,
}

impl Default for VoiceOptions {
    fn default() -> Self {
        Self { noise_suppression: true, echo_cancellation: true, input_device: None, output_device: None, voice_activation: true, volumes: Default::default() }
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

/// Собеседник в микшере: identity, его звук, громкость (1.0 — как есть)
struct Peer {
    id: String,
    cons: HeapCons<f32>,
    gain: f32,
}

type Mixer = Mutex<Vec<Peer>>;

/// Короткие звуки интерфейса голоса (синтезируются, файлов нет)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cue {
    /// Кто-то зашёл в канал (и я сам)
    Join,
    /// Кто-то вышел
    Leave,
    Mute,
    Unmute,
}

/// Два тона подряд: вверх — вход/включение, вниз — выход/выключение
fn cue_samples(cue: Cue) -> Vec<f32> {
    let (a, b, len_ms, vol) = match cue {
        Cue::Join => (660.0, 880.0, 90.0, 0.16),
        Cue::Leave => (880.0, 587.0, 90.0, 0.16),
        Cue::Unmute => (700.0, 940.0, 55.0, 0.10),
        Cue::Mute => (940.0, 700.0, 55.0, 0.10),
    };
    let n = (SAMPLE_RATE as f32 * len_ms / 1000.0) as usize;
    let mut out = Vec::with_capacity(n * 2);
    for freq in [a, b] {
        for k in 0..n {
            let t = k as f32 / SAMPLE_RATE as f32;
            // Плавные края: без щелчков
            let env = (k as f32 / (n as f32 * 0.15)).min(1.0) * ((n - k) as f32 / (n as f32 * 0.35)).min(1.0);
            out.push(vol * env * (2.0 * std::f32::consts::PI * freq * t).sin());
        }
    }
    out
}

struct Shared {
    muted: AtomicBool,
    deafened: AtomicBool,
    stop: AtomicBool,
    /// Кольца собеседников: identity → поток сэмплов
    mixer: Mixer,
    /// Громкость собеседников по identity: применяется и к тем, кто зайдёт позже
    volumes: Mutex<std::collections::HashMap<String, f32>>,
    /// Звуки интерфейса, ожидающие воспроизведения
    cues: Mutex<std::collections::VecDeque<f32>>,
    /// Чью трансляцию смотрю (identity) и последний её кадр, ещё не показанный
    watching: Mutex<Option<String>>,
    screen_frame: Mutex<Option<crate::screen::ViewFrame>>,
    /// Размер окна просмотра в пикселях: больше кадр не делаем
    view_size: Mutex<(u32, u32)>,
}

pub struct VoiceSession {
    room: Arc<Room>,
    shared: Arc<Shared>,
    audio_thread: Option<std::thread::JoinHandle<()>>,
    /// Мой показ экрана: захват и опубликованная дорожка
    screen: Arc<Mutex<Option<(crate::screen::Capture, TrackSid)>>>,
}

type ScreenSlot = Arc<Mutex<Option<(crate::screen::Capture, TrackSid)>>>;

/// Остановить свой показ: захват — сразу (поток), дорожку — снять с публикации
async fn stop_screen(room: &Room, slot: &ScreenSlot) {
    let taken = slot.lock().unwrap().take();
    if let Some((capture, sid)) = taken {
        // Остановка ждёт поток захвата — не в рантайме
        let _ = tokio::task::spawn_blocking(move || drop(capture)).await;
        let _ = room.local_participant().unpublish_track(&sid).await;
    }
}

/// Трансляция экрана участника — среди его публикаций
fn screen_publication(p: &RemoteParticipant) -> Option<RemoteTrackPublication> {
    p.track_publications().into_values().find(|t| t.source() == TrackSource::Screenshare)
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
        // Подписываемся сами: звук — всегда, чужой экран — только когда его смотрят (трафик и CPU)
        let mut room_opts = RoomOptions::default();
        room_opts.auto_subscribe = false;
        let (room, mut events) = Room::connect(url, token, room_opts)
            .await
            .map_err(|e| format!("не удалось подключиться к голосовому серверу: {e}"))?;
        let room = Arc::new(room);
        let shared = Arc::new(Shared {
            muted: AtomicBool::new(false),
            deafened: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            mixer: Mutex::new(Vec::new()),
            watching: Mutex::new(None),
            screen_frame: Mutex::new(None),
            view_size: Mutex::new((1280, 720)),
            volumes: Mutex::new(opts.volumes.clone()),
            cues: Mutex::new(std::collections::VecDeque::new()),
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
        // Кто уже в комнате: звук слушаем, о трансляциях сообщаем
        for p in room.remote_participants().values() {
            for publication in p.track_publications().values() {
                if std::env::var_os("FEATHER_LOG").is_some() {
                    eprintln!("голос: у {} дорожка {:?} {:?}", p.identity(), publication.kind(), publication.source());
                }
                match publication.kind() {
                    TrackKind::Audio => publication.set_subscribed(true),
                    TrackKind::Video if publication.source() == TrackSource::Screenshare => {
                        on_event(VoiceEvent::Screen { identity: p.identity().to_string(), live: true })
                    }
                    _ => {}
                }
            }
        }

        // Пинг и потери
        {
            let (room, shared, on_event) = (room.clone(), shared.clone(), on_event.clone());
            tokio::spawn(async move {
                let mut prev = (0u64, 0i64);
                let mut tick = tokio::time::interval(Duration::from_secs(2));
                while !shared.stop.load(Ordering::Relaxed) {
                    tick.tick().await;
                    let Ok(stats) = room.get_stats().await else { continue };
                    on_event(VoiceEvent::Stats(collect_stats(&stats, &mut prev)));
                }
            });
        }

        // События комнаты
        {
            let shared = shared.clone();
            tokio::spawn(async move {
                while let Some(ev) = events.recv().await {
                    match ev {
                        RoomEvent::TrackSubscribed { track: RemoteTrack::Audio(t), participant, .. } => {
                            play_remote(shared.clone(), participant.identity().to_string(), t);
                        }
                        RoomEvent::TrackSubscribed { track: RemoteTrack::Video(t), participant, .. } => {
                            view_remote(shared.clone(), participant.identity().to_string(), t);
                        }
                        RoomEvent::TrackPublished { publication, participant } => match publication.kind() {
                            _ if std::env::var_os("FEATHER_LOG").is_some() && {
                                eprintln!("голос: опубликована {:?} {:?} у {}", publication.kind(), publication.source(), participant.identity());
                                false
                            } => {}
                            TrackKind::Audio => publication.set_subscribed(true),
                            TrackKind::Video if publication.source() == TrackSource::Screenshare => {
                                let id = participant.identity().to_string();
                                // Уже смотрели этого человека (переподключился) — продолжаем
                                if shared.watching.lock().unwrap().as_deref() == Some(id.as_str()) {
                                    publication.set_subscribed(true);
                                }
                                on_event(VoiceEvent::Screen { identity: id, live: true });
                            }
                            _ => {}
                        },
                        RoomEvent::TrackUnpublished { publication, participant } if publication.source() == TrackSource::Screenshare => {
                            on_event(VoiceEvent::Screen { identity: participant.identity().to_string(), live: false });
                        }
                        RoomEvent::ParticipantConnected(p) => {
                            on_event(VoiceEvent::PeerJoined(VoicePeer::new(p.identity().to_string(), p.name().to_string())))
                        }
                        RoomEvent::ParticipantDisconnected(p) => {
                            let id = p.identity().to_string();
                            shared.mixer.lock().unwrap().retain(|p| p.id != id);
                            on_event(VoiceEvent::Screen { identity: id.clone(), live: false });
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

        Ok(Self { room, shared, audio_thread: Some(audio_thread), screen: Arc::new(Mutex::new(None)) })
    }

    /// Начать показ экрана. На Linux система сама спросит, что показывать.
    /// Возвращает задачу без ссылок на сессию: её можно запустить в фоне
    pub fn start_screen(
        &self,
        source: Option<crate::screen::Source>,
        quality: crate::screen::Quality,
        on_end: impl FnOnce(String) + Send + 'static,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send + 'static {
        use livekit::options::{VideoCodec, VideoEncoding};
        use livekit::webrtc::video_source::native::NativeVideoSource;
        use livekit::webrtc::video_source::{RtcVideoSource, VideoResolution};
        let (room, slot) = (self.room.clone(), self.screen.clone());
        async move {
            stop_screen(&room, &slot).await;
            let video = NativeVideoSource::new(VideoResolution { width: quality.max_width, height: quality.max_height }, true);
            let track = LocalVideoTrack::create_video_track("screen", RtcVideoSource::Native(video.clone()));
            let publication = room
                .local_participant()
                .publish_track(
                    LocalTrack::Video(track),
                    TrackPublishOptions {
                        source: TrackSource::Screenshare,
                        // H.264: WebRTC берёт аппаратный кодировщик, если он есть (D14)
                        video_codec: VideoCodec::H264,
                        simulcast: false,
                        video_encoding: Some(VideoEncoding { max_bitrate: quality.bitrate, max_framerate: quality.fps as f64 }),
                        ..Default::default()
                    },
                )
                .await
                .map_err(|e| format!("не удалось начать показ: {e}"))?;
            let capture = crate::screen::start(source, quality, video, on_end);
            *slot.lock().unwrap() = Some((capture, publication.sid()));
            Ok(())
        }
    }

    pub fn stop_screen(&self) -> impl std::future::Future<Output = ()> + Send + 'static {
        let (room, slot) = (self.room.clone(), self.screen.clone());
        async move { stop_screen(&room, &slot).await }
    }

    pub fn is_sharing(&self) -> bool {
        self.screen.lock().unwrap().is_some()
    }

    /// Смотреть трансляцию участника (None — перестать). Подписка только на одну
    pub fn watch(&self, identity: Option<&str>) {
        let mut watching = self.shared.watching.lock().unwrap();
        for p in self.room.remote_participants().values() {
            if let Some(publication) = screen_publication(p) {
                publication.set_subscribed(Some(p.identity().as_str()) == identity);
            }
        }
        *watching = identity.map(str::to_owned);
        *self.shared.screen_frame.lock().unwrap() = None;
    }

    /// Размер области просмотра (физические пиксели)
    pub fn set_view_size(&self, w: u32, h: u32) {
        *self.shared.view_size.lock().unwrap() = (w, h);
    }

    /// Последний ещё не показанный кадр трансляции
    pub fn take_screen_frame(&self) -> Option<crate::screen::ViewFrame> {
        self.shared.screen_frame.lock().unwrap().take()
    }

    /// Громкость собеседника: 0.0 — тишина, 1.0 — как есть, до 2.0 — громче
    pub fn set_volume(&self, identity: &str, gain: f32) {
        let gain = gain.clamp(0.0, 2.0);
        self.shared.volumes.lock().unwrap().insert(identity.to_owned(), gain);
        if let Some(p) = self.shared.mixer.lock().unwrap().iter_mut().find(|p| p.id == identity) {
            p.gain = gain;
        }
    }

    /// Проиграть звук интерфейса (вход, выход, микрофон)
    pub fn play_cue(&self, cue: Cue) {
        let mut q = self.shared.cues.lock().unwrap();
        if q.len() < SAMPLE_RATE as usize {
            q.extend(cue_samples(cue));
        }
    }

    pub fn set_muted(&self, muted: bool) {
        self.shared.muted.store(muted, Ordering::Relaxed);
    }

    /// «Выключить звук»: не слышим других (и, как в Discord, сами тоже молчим)
    pub fn set_deafened(&self, deafened: bool) {
        self.shared.deafened.store(deafened, Ordering::Relaxed);
    }

    pub async fn leave(mut self) {
        stop_screen(&self.room, &self.screen).await;
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
                        for p in mixer.iter_mut() {
                            let queued = p.cons.occupied_len();
                            if queued > n + MAX_QUEUED {
                                p.cons.skip(queued - n - TRIM_TO);
                            }
                            tmp.clear();
                            tmp.resize(n, 0.0);
                            let got = p.cons.pop_slice(&mut tmp);
                            for (m, s) in mono.iter_mut().zip(&tmp[..got]) {
                                *m += s * p.gain;
                            }
                        }
                    }
                    if shared.deafened.load(Ordering::Relaxed) {
                        mono.iter_mut().for_each(|s| *s = 0.0);
                    }
                    // Опорный сигнал для эхоподавления — без звуков интерфейса (их и так не слышно в микрофоне)
                    if let Ok(mut rev) = rev.try_lock() {
                        let _ = rev.push_slice(mono);
                    }
                    if let Ok(mut cues) = shared.cues.try_lock()
                        && !cues.is_empty()
                    {
                        for m in mono.iter_mut() {
                            match cues.pop_front() {
                                Some(c) => *m += c,
                                None => break,
                            }
                        }
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

            // Микрофон выключен (или PTT отпущен): вместо звука — тишина, без обработки.
            // Кадры нельзя просто пропускать: WebRTC считает время RTP по числу отправленных
            // сэмплов, и после паузы приёмник решает, что пакеты опаздывают, раздувает буфер —
            // у собеседника растёт задержка и звук рвётся. Тишину Opus с DTX почти не передаёт
            if shared.muted.load(Ordering::Relaxed) || shared.deafened.load(Ordering::Relaxed) {
                gate_open_frames = 0;
                if !send_silence(&source, &mut pcm).await {
                    return;
                }
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
                    // Тишина: отправляем нули (см. выше, почему не пропускаем кадр)
                    if !send_silence(&source, &mut pcm).await {
                        return;
                    }
                    continue;
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

/// Кадр тишины в источник: время RTP идёт ровно, Opus с DTX передаёт только редкие служебные пакеты
async fn send_silence(source: &NativeAudioSource, pcm: &mut [i16; FRAME]) -> bool {
    pcm.fill(0);
    let af = AudioFrame { data: pcm.as_slice().into(), sample_rate: SAMPLE_RATE, num_channels: 1, samples_per_channel: FRAME as u32 };
    source.capture_frame(&af).await.is_ok()
}

/// Чужая трансляция: кадры в RGBA, но только когда прошлый уже показан —
/// лишние кадры не переводим (экономия CPU, если окно не успевает)
fn view_remote(shared: Arc<Shared>, identity: String, track: RemoteVideoTrack) {
    tokio::spawn(async move {
        let mut stream = livekit::webrtc::video_stream::native::NativeVideoStream::new(track.rtc_track());
        while let Some(frame) = stream.next().await {
            if shared.stop.load(Ordering::Relaxed) || shared.watching.lock().unwrap().as_deref() != Some(identity.as_str()) {
                break;
            }
            if shared.screen_frame.lock().unwrap().is_some() {
                continue;
            }
            let max = *shared.view_size.lock().unwrap();
            let view = crate::screen::to_rgba(frame.buffer.as_ref(), max);
            *shared.screen_frame.lock().unwrap() = Some(view);
        }
    });
}

/// Звук собеседника: декодированные кадры LiveKit → его кольцо в микшере
fn play_remote(shared: Arc<Shared>, identity: String, track: RemoteAudioTrack) {
    let (mut prod, cons) = HeapRb::<f32>::new(RING).split();
    {
        let gain = shared.volumes.lock().unwrap().get(&identity).copied().unwrap_or(1.0);
        let mut mixer = shared.mixer.lock().unwrap();
        mixer.retain(|p| p.id != identity);
        mixer.push(Peer { id: identity, cons, gain });
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

/// Пинг по выбранной ICE-паре, протокол по локальному кандидату, потери по входящему звуку
fn collect_stats(stats: &livekit::rtc_engine::SessionStats, prev: &mut (u64, i64)) -> VoiceStats {
    use livekit::webrtc::stats::{IceCandidatePairState, IceCandidateType, RtcStats};
    let mut out = VoiceStats::default();
    // Пинг и протокол — по каналу отправки (он есть всегда, пока мы в комнате), иначе по приёму
    for list in [&stats.publisher_stats, &stats.subscriber_stats] {
        let selected: Vec<&str> = list
            .iter()
            .filter_map(|s| match s {
                RtcStats::Transport(t) if !t.transport.selected_candidate_pair_id.is_empty() => Some(t.transport.selected_candidate_pair_id.as_str()),
                _ => None,
            })
            .collect();
        let pair = list.iter().find_map(|s| match s {
            RtcStats::CandidatePair(p)
                if selected.contains(&p.rtc.id.as_str())
                    || (selected.is_empty() && p.candidate_pair.nominated && p.candidate_pair.state == Some(IceCandidatePairState::Succeeded)) =>
            {
                Some(&p.candidate_pair)
            }
            _ => None,
        });
        let Some(pair) = pair else { continue };
        if pair.current_round_trip_time > 0.0 {
            out.rtt_ms = Some((pair.current_round_trip_time * 1000.0).round() as u32);
        }
        out.transport = list
            .iter()
            .find_map(|s| match s {
                RtcStats::LocalCandidate(c) if c.rtc.id == pair.local_candidate_id => Some(if c.local_candidate.candidate_type == Some(IceCandidateType::Relay) {
                    "TURN".to_owned()
                } else {
                    c.local_candidate.protocol.to_uppercase()
                }),
                _ => None,
            })
            .unwrap_or_default();
        break;
    }
    let (mut received, mut lost) = (0u64, 0i64);
    // Одно соединение на всё (новые LiveKit) — входящий звук может быть и в «publisher»
    for s in stats.subscriber_stats.iter().chain(&stats.publisher_stats) {
        if let RtcStats::InboundRtp(r) = s {
            if r.stream.kind == "audio" {
                received += r.received.packets_received;
                lost += r.received.packets_lost;
            }
        }
    }
    let (d_recv, d_lost) = (received.saturating_sub(prev.0), (lost - prev.1).max(0));
    if received >= prev.0 && d_recv + d_lost as u64 > 0 {
        out.loss_pct = d_lost as f32 * 100.0 / (d_recv + d_lost as u64) as f32;
    }
    *prev = (received, lost);
    out
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
