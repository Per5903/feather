#!/usr/bin/env bash
# Виртуальные аудиоустройства PipeWire/PulseAudio для голосовых тестов.
#   vdev.sh up    — создать виртуальный микрофон (feather_vmic) и динамики (feather_vout)
#   vdev.sh play  — проигрывать зашумлённую речь в виртуальный микрофон по кругу
#   vdev.sh stop  — остановить проигрывание
#   vdev.sh down  — удалить всё созданное
# Устройства по умолчанию в системе не меняются.
set -euo pipefail
cd "$(dirname "$0")"
STATE=/tmp/feather-vdev-modules
PLAY_PID=/tmp/feather-vdev-play.pid

stop_play() {
  [ -f "$PLAY_PID" ] && kill "$(cat "$PLAY_PID")" 2>/dev/null || true
  rm -f "$PLAY_PID"
}

case "${1:-}" in
  up)
    if pactl list short sinks | grep -q feather_vmic_sink; then
      echo "виртуальные устройства уже созданы"; exit 0
    fi
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
    echo $$ > "$PLAY_PID"
    trap 'kill $(jobs -p) 2>/dev/null; rm -f "$PLAY_PID"; exit 0' TERM INT
    while true; do pw-play --target feather_vmic_sink "$file" & wait $!; done
    ;;
  stop)
    stop_play
    ;;
  down)
    stop_play
    [ -f "$STATE" ] && tac "$STATE" | xargs -r -n1 pactl unload-module || true
    rm -f "$STATE"
    echo "виртуальные устройства удалены"
    ;;
  *) echo "использование: $0 up | play [файл] | stop | down"; exit 1 ;;
esac
