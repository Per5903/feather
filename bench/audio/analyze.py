#!/usr/bin/env python3
"""Оценка шумодава по принятой записи, без выравнивания по исходнику.

Кадры по 20 мс сортируются по громкости:
  фон  — 10-й перцентиль (паузы между фразами: что осталось от шума);
  речь — 90-й перцентиль;
  разница — насколько речь громче фона (не зависит от общей громкости и AGC).
Кадры «цифровой тишины» (< -90 дБ) считаются отдельно и в фон не входят.
Использование: analyze.py запись.wav [ещё.wav ...]
"""
import sys
import wave
from pathlib import Path

import numpy as np

HERE = Path(__file__).parent
SR = 48_000
FRAME = SR // 50  # окна по 20 мс


def load(path):
    with wave.open(str(path)) as w:
        assert w.getframerate() == SR and w.getnchannels() == 1
        return np.frombuffer(w.readframes(w.getnframes()), dtype=np.int16).astype(np.float64) / 32768


def frames_rms(x):
    n = len(x) // FRAME
    return np.sqrt(np.mean(x[: n * FRAME].reshape(n, FRAME) ** 2, axis=1) + 1e-12)


def db(x):
    return 20 * np.log10(x + 1e-12)


def report(name, x):
    d = db(frames_rms(x))
    silent = np.mean(d < -90) * 100
    # Цифровую тишину (участник ещё не подключился / уже вышел) в фон не включаем
    d = d[d >= -90]
    floor = np.percentile(d, 10)
    speech = np.percentile(d, 90)
    print(f"{name:28s} фон {floor:7.1f} дБ   речь {speech:6.1f} дБ   разница {speech - floor:5.1f} дБ   тишина {silent:4.1f} %")


def main():
    report("чистая речь (эталон)", load(HERE / "speech_clean.wav"))
    report("шумный микрофон (вход)", load(HERE / "speech_noisy.wav"))
    for p in sys.argv[1:]:
        # первые 5 с — подключение, разгон AGC и шумодава
        report(Path(p).stem, load(p)[SR * 5:])


if __name__ == "__main__":
    main()
