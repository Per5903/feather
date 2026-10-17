//! Замер на приёмнике: задержка буфера приёма (jitter buffer) и доля «додуманного» звука
//! (concealment — это и есть обрывки). Отправитель — настоящий голосовой тракт Feather
//! (активация голосом), микрофон — виртуальный (bench/audio/vdev.sh up/play).
//!
//! Запуск: cargo run -p feather-voice-proto --bin jitter --release [секунды]

use std::time::Duration;

use feather_core::{VoiceOptions, VoiceSession};
use futures_util::StreamExt;
use livekit::prelude::*;
use livekit::webrtc::audio_stream::native::NativeAudioStream;
use livekit::webrtc::stats::RtcStats;
use livekit_api::access_token::{AccessToken, VideoGrants};

fn token(identity: &str) -> String {
    AccessToken::with_api_key("devkey", "dev_secret_change_me_at_least_32_chars_long")
        .with_identity(identity)
        .with_grants(VideoGrants { room_join: true, room: "jitter-probe".into(), ..Default::default() })
        .to_jwt()
        .unwrap()
}

#[tokio::main]
async fn main() {
    feather_core::tls::init();
    let secs: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(60);
    let url = "ws://127.0.0.1:7880";

    let (probe, mut events) = Room::connect(url, &token("probe"), RoomOptions::default()).await.expect("LiveKit");
    tokio::spawn(async move {
        while let Some(ev) = events.recv().await {
            if let RoomEvent::TrackSubscribed { track: RemoteTrack::Audio(t), .. } = ev {
                // Читаем звук, как настоящий клиент: иначе декодер и буфер приёма не работают
                tokio::spawn(async move {
                    let mut s = NativeAudioStream::new(t.rtc_track(), 48_000, 1);
                    while s.next().await.is_some() {}
                });
            }
        }
    });

    let opts = VoiceOptions {
        input_device: Some("FeatherVirtualMic".into()),
        output_device: Some("FeatherVirtualOut".into()),
        ..Default::default()
    };
    let _sender = VoiceSession::join(url, &token("sender"), opts, |_| {}).await.expect("отправитель");

    let (mut prev_delay, mut prev_emitted, mut prev_conc, mut prev_total) = (0.0f64, 0u64, 0u64, 0u64);
    println!("  t,с  буфер_мс  додумано_%");
    for t in (2..=secs).step_by(2) {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let Ok(stats) = probe.get_stats().await else { continue };
        if std::env::var_os("JITTER_DEBUG").is_some() {
            for s in stats.subscriber_stats.iter().chain(&stats.publisher_stats) {
                if let RtcStats::InboundRtp(r) = s {
                    eprintln!("inbound kind={:?} emitted={}", r.stream.kind, r.inbound.jitter_buffer_emitted_count);
                }
            }
            eprintln!("subscriber={} participants={}", stats.subscriber_stats.len(), probe.remote_participants().len());
        }
        for s in stats.subscriber_stats.iter().chain(&stats.publisher_stats) {
            if let RtcStats::InboundRtp(r) = s
                && r.stream.kind == "audio"
            {
                let (d, e) = (r.inbound.jitter_buffer_delay, r.inbound.jitter_buffer_emitted_count);
                // Додуманное в паузах (DTX) не считаем: там и так тишина
                let (c, n) = (r.inbound.concealed_samples - r.inbound.silent_concealed_samples, r.inbound.total_samples_received);
                let buf = if e > prev_emitted { (d - prev_delay) / (e - prev_emitted) as f64 * 1000.0 } else { 0.0 };
                let conc = if n > prev_total { (c - prev_conc) as f64 * 100.0 / (n - prev_total) as f64 } else { 0.0 };
                println!("{t:5}  {buf:8.0}  {conc:9.1}");
                (prev_delay, prev_emitted, prev_conc, prev_total) = (d, e, c, n);
            }
        }
    }
}
