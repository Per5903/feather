//! Демонстрация экрана: захват (WebRTC DesktopCapturer: на Linux — портал и PipeWire,
//! на Windows — системный захват) → I420 → видеодорожка LiveKit. И обратное: кадр
//! чужой трансляции → RGBA для показа.
//!
//! Кодек H.264: WebRTC сам берёт аппаратный кодировщик (NVENC, VA-API, QuickSync),
//! без него — программный (docs/09-decisions.md, D14).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use livekit::webrtc::desktop_capturer::{CaptureError, DesktopCaptureSourceType, DesktopCapturer, DesktopCapturerOptions};
use livekit::webrtc::native::yuv_helper;
use livekit::webrtc::prelude::{I420Buffer, VideoBuffer, VideoFrame, VideoRotation};
use livekit::webrtc::video_source::native::NativeVideoSource;

/// Качество трансляции: потолок размера, частота кадров, битрейт
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Quality {
    pub max_width: u32,
    pub max_height: u32,
    pub fps: u32,
    pub bitrate: u64,
}

pub const QUALITIES: [(&str, Quality); 3] = [
    ("720p, 30 кадров", Quality { max_width: 1280, max_height: 720, fps: 30, bitrate: 2_500_000 }),
    ("1080p, 30 кадров", Quality { max_width: 1920, max_height: 1080, fps: 30, bitrate: 4_500_000 }),
    ("1080p, 60 кадров", Quality { max_width: 1920, max_height: 1080, fps: 60, bitrate: 7_000_000 }),
];

/// Что показывать. На Linux выбирает системный диалог (портал), тогда `None`
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    pub id: u64,
    pub title: String,
    pub window: bool,
}

/// Экраны и окна для выбора в Feather (Windows). На Linux пусто: выбор — в окне системы
pub fn sources() -> Vec<Source> {
    #[cfg(target_os = "linux")]
    {
        Vec::new()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let mut out = Vec::new();
        for (kind, window) in [(DesktopCaptureSourceType::Screen, false), (DesktopCaptureSourceType::Window, true)] {
            if let Some(c) = DesktopCapturer::new(DesktopCapturerOptions::new(kind)) {
                for (i, s) in c.get_source_list().into_iter().enumerate() {
                    let title = if window { s.title() } else { format!("Экран {}", i + 1) };
                    if !title.trim().is_empty() {
                        out.push(Source { id: s.id(), title, window });
                    }
                }
            }
        }
        out
    }
}

/// Размер под потолок с сохранением пропорций; чётный (требование I420)
pub fn fit(w: u32, h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    let k = (max_w as f64 / w as f64).min(max_h as f64 / h as f64).min(1.0);
    let even = |v: f64| ((v as u32) & !1).max(2);
    (even(w as f64 * k), even(h as f64 * k))
}

/// Идёт захват: поток с DesktopCapturer. Остановка — при уничтожении
pub struct Capture {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Запустить захват. `on_end` — захват прекратился сам (отказ в диалоге, окно закрыто)
pub fn start(source: Option<Source>, quality: Quality, video: NativeVideoSource, on_end: impl FnOnce(String) + Send + 'static) -> Capture {
    let stop = Arc::new(AtomicBool::new(false));
    let thread = {
        let stop = stop.clone();
        std::thread::Builder::new()
            .name("feather-screen".into())
            .spawn(move || capture_loop(source, quality, video, stop, on_end))
            .expect("поток захвата экрана")
    };
    Capture { stop, thread: Some(thread) }
}

fn capture_loop(source: Option<Source>, quality: Quality, video: NativeVideoSource, stop: Arc<AtomicBool>, on_end: impl FnOnce(String)) {
    #[cfg(target_os = "linux")]
    let kind = DesktopCaptureSourceType::Generic; // экран или окно — выбирают в окне системы
    #[cfg(not(target_os = "linux"))]
    let kind = if source.as_ref().is_some_and(|s| s.window) { DesktopCaptureSourceType::Window } else { DesktopCaptureSourceType::Screen };
    let mut opts = DesktopCapturerOptions::new(kind);
    opts.set_include_cursor(true);
    let Some(mut capturer) = DesktopCapturer::new(opts) else {
        on_end("захват экрана недоступен в этой системе".into());
        return;
    };
    let picked = source.and_then(|s| capturer.get_source_list().into_iter().find(|c| c.id() == s.id));

    // Колбэк вызывается внутри capture_frame() на этом же потоке
    let failed = Arc::new(AtomicBool::new(false));
    let start_at = Instant::now();
    {
        let failed = failed.clone();
        capturer.start_capture(picked, move |res| match res {
            Ok(frame) => {
                let (w, h) = (frame.width().max(2) as u32 & !1, frame.height().max(2) as u32 & !1);
                let mut full = I420Buffer::new(w, h);
                let (sy, su, sv) = full.strides();
                let (y, u, v) = full.data_mut();
                // BGRA в памяти — это «ARGB» в терминах libyuv
                yuv_helper::argb_to_i420(frame.data(), frame.stride(), y, sy, u, su, v, sv, w as i32, h as i32);
                let (tw, th) = fit(w, h, quality.max_width, quality.max_height);
                let buffer = if (tw, th) == (w, h) { full } else { full.scale(tw as i32, th as i32) };
                let mut vf = VideoFrame::new(VideoRotation::VideoRotation0, buffer);
                vf.timestamp_us = start_at.elapsed().as_micros() as i64;
                video.capture_frame(&vf);
            }
            Err(CaptureError::Permanent) => failed.store(true, Ordering::Relaxed),
            Err(CaptureError::Temporary) => {}
        });
    }

    let period = Duration::from_micros(1_000_000 / quality.fps.max(1) as u64);
    let mut next = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        capturer.capture_frame();
        if failed.load(Ordering::Relaxed) {
            on_end("показ экрана остановлен системой или отменён".into());
            return;
        }
        next += period;
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else {
            next = now; // не догоняем пропущенные кадры
        }
    }
}

/// Кадр чужой трансляции для показа
pub struct ViewFrame {
    pub width: u32,
    pub height: u32,
    /// RGBA, без отступов между строками
    pub rgba: Vec<u8>,
}

/// I420 → RGBA, уменьшив до `max` (размер окна просмотра): перевод и показ
/// пропорциональны числу пикселей, а показывать больше, чем помещается, незачем
pub fn to_rgba(buffer: &dyn VideoBuffer, max: (u32, u32)) -> ViewFrame {
    let mut i420 = buffer.to_i420();
    let (tw, th) = fit(i420.width(), i420.height(), max.0.max(320), max.1.max(180));
    if (tw, th) != (i420.width(), i420.height()) {
        i420 = i420.scale(tw as i32, th as i32);
    }
    let (w, h) = (i420.width(), i420.height());
    let (sy, su, sv) = i420.strides();
    let (y, u, v) = i420.data();
    let mut rgba = vec![0u8; (w * h * 4) as usize];
    // «ABGR» в терминах libyuv — это RGBA в памяти
    yuv_helper::i420_to_abgr(y, sy, u, su, v, sv, &mut rgba, w * 4, w as i32, h as i32);
    ViewFrame { width: w, height: h, rgba }
}

#[cfg(test)]
mod tests {
    use super::fit;

    #[test]
    fn fitting() {
        assert_eq!(fit(2560, 1440, 1920, 1080), (1920, 1080));
        assert_eq!(fit(1280, 720, 1920, 1080), (1280, 720), "меньше потолка — не растягиваем");
        assert_eq!(fit(3440, 1440, 1920, 1080), (1920, 802));
        assert_eq!(fit(1001, 601, 1920, 1080), (1000, 600), "чётные стороны");
    }
}
