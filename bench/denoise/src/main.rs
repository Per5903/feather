//! Прогоняет wav (48 кГц, моно) через шумодав и пишет результат.
//! Печатает, какую долю одного ядра шумодав занял бы в реальном времени.
//!
//!   cargo run --release -p feather-denoise-bench -- rnnoise вход.wav выход.wav

use std::time::Instant;

use nnnoiseless::DenoiseState;

fn read(path: &str) -> Vec<f32> {
    let mut r = hound::WavReader::open(path).expect("не открыть wav");
    let spec = r.spec();
    assert_eq!((spec.sample_rate, spec.channels), (48_000, 1), "нужен 48 кГц моно");
    r.samples::<i16>().map(|s| s.unwrap() as f32).collect()
}

fn write(path: &str, data: &[f32]) {
    let spec = hound::WavSpec { channels: 1, sample_rate: 48_000, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
    let mut w = hound::WavWriter::create(path, spec).expect("не создать wav");
    for s in data {
        w.write_sample(s.clamp(-32768.0, 32767.0) as i16).unwrap();
    }
    w.finalize().unwrap();
}

fn rnnoise(input: &[f32]) -> Vec<f32> {
    // RNNoise работает кадрами по 10 мс (480 сэмплов) в шкале i16
    let mut st = DenoiseState::new();
    let mut out = Vec::with_capacity(input.len());
    let mut frame_out = [0f32; DenoiseState::FRAME_SIZE];
    for chunk in input.chunks(DenoiseState::FRAME_SIZE) {
        let mut frame = [0f32; DenoiseState::FRAME_SIZE];
        frame[..chunk.len()].copy_from_slice(chunk);
        st.process_frame(&mut frame_out, &frame);
        out.extend_from_slice(&frame_out[..chunk.len()]);
    }
    out
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [algo, inp, outp] = args.as_slice() else {
        eprintln!("использование: feather-denoise-bench rnnoise вход.wav выход.wav");
        std::process::exit(2);
    };
    let input = read(inp);
    let audio_secs = input.len() as f64 / 48_000.0;
    let t = Instant::now();
    let out = match algo.as_str() {
        "rnnoise" => rnnoise(&input),
        other => panic!("неизвестный шумодав {other}"),
    };
    let cpu = t.elapsed().as_secs_f64();
    write(outp, &out);
    println!(
        "{algo}: {audio_secs:.1} с звука обработано за {cpu:.2} с → в реальном времени ≈ {:.2} % одного ядра",
        cpu / audio_secs * 100.0
    );
}
