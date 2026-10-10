//! Прототип голоса этапа 0 (docs/06-voice.md).
//!
//! Режимы:
//!   latency      — в одном процессе два участника: один пищит раз в секунду,
//!                  другой ловит писк. Печатает задержку «отправка → декодированный звук».
//!   peer <имя>   — обычный участник голосового канала: шлёт «речь» и принимает всех.
//!                  Запускается в нескольких процессах для замера CPU/RAM (bench/measure.py).
//!
//! Звук синтетический (без микрофона и динамиков), чтобы тест шёл без человека.
//! Реальные устройства в SDK есть (livekit::PlatformAudio), они следующий шаг.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use livekit::options::TrackPublishOptions;
use livekit::prelude::*;
use livekit::webrtc::audio_frame::AudioFrame;
use livekit::webrtc::audio_source::AudioSourceOptions;
use livekit::webrtc::audio_source::native::NativeAudioSource;
use livekit::webrtc::audio_stream::native::NativeAudioStream;
use livekit_api::access_token::{AccessToken, VideoGrants};

const URL: &str = "ws://127.0.0.1:7880";
const ROOM: &str = "voice-bench";
const SAMPLE_RATE: u32 = 48_000;
const FRAME_MS: u32 = 10;
const SAMPLES: usize = (SAMPLE_RATE * FRAME_MS / 1000) as usize;
/// Порог громкости (RMS, i16), выше которого считаем, что писк дошёл
const ONSET_RMS: f64 = 2_000.0;

type Onsets = Arc<Mutex<VecDeque<Instant>>>;

fn token(identity: &str) -> String {
    let key = std::env::var("LIVEKIT_API_KEY").unwrap_or_else(|_| "devkey".into());
    let secret = std::env::var("LIVEKIT_API_SECRET")
        .unwrap_or_else(|_| "dev_secret_change_me_at_least_32_chars_long".into());
    AccessToken::with_api_key(&key, &secret)
        .with_identity(identity)
        .with_grants(VideoGrants { room_join: true, room: ROOM.into(), ..Default::default() })
        .to_jwt()
        .expect("не удалось подписать токен")
}

async fn join(identity: &str) -> (Room, tokio::sync::mpsc::UnboundedReceiver<RoomEvent>) {
    Room::connect(URL, &token(identity), RoomOptions::default())
        .await
        .unwrap_or_else(|e| panic!("{identity}: не удалось подключиться к LiveKit: {e}"))
}

/// Публикует микрофон-заглушку. `sound(t)` решает, что звучит в кадре, начатом в момент t.
async fn publish(room: &Room, mut sound: impl FnMut(Instant, u64) -> bool + Send + 'static) -> LocalAudioTrack {
    // AEC/NS/AGC выключены: синтетический сигнал, обработка исказила бы замер
    let source = NativeAudioSource::new(AudioSourceOptions::default(), SAMPLE_RATE, 1, 0);
    let track = LocalAudioTrack::create_audio_track("mic", RtcAudioSource::Native(source.clone()));
    room.local_participant()
        .publish_track(
            LocalTrack::Audio(track.clone()),
            TrackPublishOptions { source: TrackSource::Microphone, dtx: true, ..Default::default() },
        )
        .await
        .expect("не удалось опубликовать трек");

    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_millis(FRAME_MS as u64));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut n: u64 = 0;
        let mut buf = vec![0i16; SAMPLES];
        // Как при отпущенной кнопке push-to-talk без отправки кадров: проверяем, затихнет ли кодировщик
        let no_feed = std::env::var_os("FEATHER_NO_FEED").is_some();
        loop {
            tick.tick().await;
            if no_feed {
                continue;
            }
            let loud = sound(Instant::now(), n);
            for (i, s) in buf.iter_mut().enumerate() {
                let t = (n as usize * SAMPLES + i) as f64 / SAMPLE_RATE as f64;
                *s = if loud { ((t * 1000.0 * std::f64::consts::TAU).sin() * 12_000.0) as i16 } else { 0 };
            }
            let frame = AudioFrame {
                data: buf.as_slice().into(),
                sample_rate: SAMPLE_RATE,
                num_channels: 1,
                samples_per_channel: SAMPLES as u32,
            };
            if source.capture_frame(&frame).await.is_err() {
                break;
            }
            n += 1;
        }
    });
    track
}

/// Принимает чужой звук. Если заданы onsets — ловит начало писка и печатает задержку.
fn consume(track: RemoteAudioTrack, onsets: Option<Onsets>, latencies: Arc<Mutex<Vec<f64>>>) {
    tokio::spawn(async move {
        let mut stream = NativeAudioStream::new(track.rtc_track(), SAMPLE_RATE as i32, 1);
        let mut was_loud = false;
        while let Some(frame) = stream.next().await {
            let Some(onsets) = &onsets else { continue };
            let rms = (frame.data.iter().map(|&s| (s as f64).powi(2)).sum::<f64>()
                / frame.data.len().max(1) as f64)
                .sqrt();
            let loud = rms > ONSET_RMS;
            if loud && !was_loud {
                if let Some(sent) = onsets.lock().unwrap().pop_front() {
                    let ms = sent.elapsed().as_secs_f64() * 1000.0;
                    println!("писк дошёл за {ms:.1} мс");
                    latencies.lock().unwrap().push(ms);
                }
            }
            was_loud = loud;
        }
    });
}

async fn latency_mode() {
    let onsets: Onsets = Arc::default();
    let latencies: Arc<Mutex<Vec<f64>>> = Arc::default();

    let (listener, mut events) = join("listener").await;
    let (speaker, _speaker_events) = join("speaker").await;

    let recv_onsets = onsets.clone();
    let recv_lat = latencies.clone();
    tokio::spawn(async move {
        while let Some(ev) = events.recv().await {
            if let RoomEvent::TrackSubscribed { track: RemoteTrack::Audio(t), .. } = ev {
                consume(t, Some(recv_onsets.clone()), recv_lat.clone());
            }
        }
    });

    // Писк 100 мс раз в секунду; момент начала писка кладём в очередь
    let send_onsets = onsets.clone();
    publish(&speaker, move |now, n| {
        let phase = n % 100;
        if phase == 0 {
            // Игнорируем писки, которые никто не поймал (например, до подписки)
            let mut q = send_onsets.lock().unwrap();
            q.retain(|t| now.duration_since(*t) < Duration::from_millis(900));
            q.push_back(now);
        }
        phase < 10
    })
    .await;

    tokio::time::sleep(Duration::from_secs(25)).await;
    let mut v = latencies.lock().unwrap().clone();
    // Первые писки идут, пока джиттер-буфер «разгоняется», их не считаем
    if v.len() > 3 {
        v.drain(..3);
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    if v.is_empty() {
        println!("ИТОГ: ни один писк не дошёл");
    } else {
        println!(
            "ИТОГ ({} замеров): мин {:.1} мс, медиана {:.1} мс, макс {:.1} мс",
            v.len(),
            v[0],
            v[v.len() / 2],
            v[v.len() - 1]
        );
    }
    listener.close().await.ok();
    speaker.close().await.ok();
}

async fn peer_mode(name: String) {
    let (room, mut events) = join(&name).await;
    // «Речь»: 3 секунды говорим, 2 молчим (DTX экономит трафик на паузах)
    // Переключатели для поиска, куда уходит CPU (bench/README.md)
    let silent = std::env::var_os("FEATHER_SILENT").is_some();
    let no_sub = std::env::var_os("FEATHER_NO_CONSUME").is_some();
    let track = publish(&room, move |_, n| !silent && n % 500 < 300).await;
    // Как при отпущенной кнопке push-to-talk
    if std::env::var_os("FEATHER_MUTED").is_some() {
        track.mute();
    }
    let latencies = Arc::default();
    println!("{name}: в канале");
    while let Some(ev) = events.recv().await {
        if let RoomEvent::TrackSubscribed { track: RemoteTrack::Audio(t), participant, .. } = ev {
            println!("{name}: слышу {}", participant.identity());
            if !no_sub {
                consume(t, None, Arc::clone(&latencies));
            }
        }
    }
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("latency") => latency_mode().await,
        Some("peer") => peer_mode(args.get(1).cloned().unwrap_or_else(|| "peer".into())).await,
        _ => eprintln!("использование: feather-voice-proto latency | peer <имя>"),
    }
}
