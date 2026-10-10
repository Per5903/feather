#!/usr/bin/env python3
"""Готовит тестовый звук для голосовых замеров (48 кГц, моно):
  speech_clean.wav — речь из bench/audio/src;
  speech_noisy.wav — та же речь + розовый шум + гул 50 Гц + щелчки клавиатуры.
Seed фиксирован: файл одинаковый при каждой генерации.
"""
import subprocess
import wave
from pathlib import Path

import numpy as np

SR = 48_000
HERE = Path(__file__).parent


def load(path):
    raw = subprocess.run(
        ["ffmpeg", "-v", "error", "-i", str(path), "-ac", "1", "-ar", str(SR), "-f", "s16le", "-"],
        check=True, capture_output=True,
    ).stdout
    return np.frombuffer(raw, dtype=np.int16).astype(np.float64) / 32768


def save(path, x):
    with wave.open(str(path), "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(SR)
        w.writeframes((np.clip(x, -1, 1) * 32767).astype(np.int16).tobytes())


def pink(n, rng):
    # Розовый шум через спектр 1/sqrt(f)
    spec = np.fft.rfft(rng.standard_normal(n))
    f = np.fft.rfftfreq(n, 1 / SR)
    spec[1:] /= np.sqrt(f[1:])
    spec[0] = 0
    x = np.fft.irfft(spec, n)
    return x / np.max(np.abs(x))


def main():
    rng = np.random.default_rng(7)
    speech = np.concatenate([load(p) for p in sorted((HERE / "src").glob("*.wav"))])
    speech *= 0.5 / np.max(np.abs(speech))
    n = len(speech)
    t = np.arange(n) / SR

    noise = 0.03 * pink(n, rng)                                   # вентилятор/фон
    noise += 0.015 * np.sin(2 * np.pi * 50 * t)                     # гул сети
    clicks = np.zeros(n)                                           # клавиатура
    for pos in rng.integers(0, n - 2000, size=int(n / SR * 4)):
        burst = rng.standard_normal(600) * np.exp(-np.arange(600) / 80)
        clicks[pos:pos + 600] += 0.12 * burst
    noise += clicks

    save(HERE / "speech_clean.wav", speech)
    save(HERE / "speech_noisy.wav", speech + noise)
    snr = 10 * np.log10(np.mean(speech ** 2) / np.mean(noise ** 2))
    print(f"длительность {n / SR:.1f} с, SNR зашумлённой версии {snr:.1f} дБ")


if __name__ == "__main__":
    main()
