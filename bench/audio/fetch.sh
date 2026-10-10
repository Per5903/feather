#!/usr/bin/env bash
# Тестовая речь: Open Speech Repository (фразы Harvard, свободно для тестов VoIP).
# Скачивается в bench/audio/src/, в git не хранится.
set -euo pipefail
cd "$(dirname "$0")"
mkdir -p src
for n in 0010 0030; do
  f="src/OSR_us_000_${n}_8k.wav"
  [ -f "$f" ] || curl -fsSL -o "$f" "https://www.voiptroubleshooter.com/open_speech/american/OSR_us_000_${n}_8k.wav"
done
ls -la src
