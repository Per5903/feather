#!/usr/bin/env bash
# Виртуальные аудиоустройства PipeWire/PulseAudio для голосовых тестов.
#   vdev.sh up    — создать виртуальный микрофон (feather_vmic) и динамики (feather_vout)
#   vdev.sh play  — проигрывать зашумлённую речь в виртуальный микрофон по кругу
#   vdev.sh down  — удалить всё созданное
# Устройства по умолчанию в системе не меняются.
set -euo pipefail
cd "$(dirname "$0")"
STATE=/tmp/feather-vdev-modules

case "${1:-}" in
  up)
    : > "$STATE"
    pactl load-module module-null-sink sink_name=feather_vmic_sink \
      sink_properties=device.description=FeatherVirtualMicSink >> "$STATE"
    pactl load-module module-remap-source master=feather_vmic_sink.monitor source_name=feather_vmic \
      source_properties=device.description=FeatherVirtualMic >> "$STATE"
    pactl load-module module-null-sink sink_name=feather_vout \
      sink_properties=device.description=FeatherVirtualOut >> "$STATE"
    echo "созданы модули: $(tr '\n' ' ' < "$STATE")"
    ;;
  play)
    file="${2:-speech_noisy.wav}"
    while true; do pw-play --target feather_vmic_sink "$file"; done
    ;;
  down)
    [ -f "$STATE" ] && tac "$STATE" | xargs -r -n1 pactl unload-module || true
    rm -f "$STATE"
    echo "виртуальные устройства удалены"
    ;;
  *) echo "использование: $0 up | play [файл] | down"; exit 1 ;;
esac
