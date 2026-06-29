#!/usr/bin/env bash
# Set up Joya's PipeWire audio routing so it never listens to its own output.
#
# This creates two null sinks plus a loopback:
#
#   joya_mic     — Joya plays the `self` direction's TTS here. Select
#                  `joya_mic.monitor` as the microphone in your call app so the
#                  other party hears your translated speech.
#
#   call_remote  — The call app plays the OTHER party's audio here (set the
#                  call's output device to "Call_Remote"). Joya captures
#                  `call_remote.monitor` for the `relay` direction, so relay
#                  hears *only* the other party — never Joya's own TTS.
#
#   loopback     — `call_remote.monitor` → your headphones, so you still hear
#                  the other party. Joya's TTS also goes to your headphones but
#                  never to `call_remote`, so there is no path from Joya's
#                  output back into its `relay` capture.
#
# Net effect: `relay` captures {other party} and plays {translation} to your
# headphones; the two never mix on the capture side, so Joya doesn't re-translate
# its own voice. It stays full-duplex — you can talk over each other. This is
# the recommended setup; without it, `relay`'s default capture (the headphone
# monitor) hears Joya's TTS too and loops.
#
# PipeWire modules don't persist across reboots, so run this once per session.
#
# Usage:
#   scripts/setup-virtual-mic.sh           # create the routing
#   scripts/setup-virtual-mic.sh teardown  # remove it
#
# Optional:
#   JOYA_LISTEN_SINK=<sink> scripts/setup-virtual-mic.sh
#       Hear the other party on a specific sink instead of your default output
#       (e.g. when your default isn't your headphones). Use a sink name from
#       `pactl list short sinks`.
#
# After loading, in Joya's config.yaml:
#   audio:
#     relay:
#       capture_device: call_remote
#     output:
#       mic_sink: joya_mic
# And in your call app (Discord/Zoom/…):
#   - Output device → "Call_Remote"
#   - Microphone    → "Monitor of Joya_Mic"

set -euo pipefail

MIC_SINK="joya_mic"
REMOTE_SINK="call_remote"
# Where the loopback sends the other party's audio so you can hear them.
# @DEFAULT_SINK@ resolves to your default output (your headphones) at load time.
LISTEN_SINK="${JOYA_LISTEN_SINK:-@DEFAULT_SINK@}"

# Unload every module whose argument line contains `pattern` (literal substring).
unload_matching() {
    local pattern="$1"
    pactl list short modules \
        | awk -v p="$pattern" 'index($0, p) > 0 {print $1}' \
        | while read -r id; do pactl unload-module "$id"; done
}

# True if a sink named `$1` exists.
sink_exists() {
    pactl list short sinks | awk '{print $2}' | grep -Fxq "$1"
}

# True if a module with `pattern` in its argument is loaded.
module_exists() {
    pactl list short modules | grep -Fq "$1"
}

if [[ "${1:-}" == "teardown" ]]; then
    # Loopback first (it reads from call_remote.monitor), then the sinks.
    unload_matching "source=${REMOTE_SINK}.monitor"
    unload_matching "sink_name=${MIC_SINK}"
    unload_matching "sink_name=${REMOTE_SINK}"
    echo "Removed Joya audio routing (virtual mic, call isolation sink, loopback)."
    exit 0
fi

# 1. Virtual mic for the `self` direction (your speech → the call).
if sink_exists "${MIC_SINK}"; then
    echo "Virtual mic '${MIC_SINK}' already exists."
else
    pactl load-module module-null-sink \
        sink_name="${MIC_SINK}" \
        sink_properties=device.description="Joya_Mic" >/dev/null
    echo "Created virtual mic '${MIC_SINK}'."
fi

# 2. Isolation sink for the other party (call output → relay capture).
#    Joya's TTS never reaches this sink, so its monitor is loop-free.
if sink_exists "${REMOTE_SINK}"; then
    echo "Call isolation sink '${REMOTE_SINK}' already exists."
else
    pactl load-module module-null-sink \
        sink_name="${REMOTE_SINK}" \
        sink_properties=device.description="Call_Remote" >/dev/null
    echo "Created call isolation sink '${REMOTE_SINK}'."
fi

# 3. Still hear the other party: loop their audio to your headphones.
if module_exists "source=${REMOTE_SINK}.monitor"; then
    echo "Loopback '${REMOTE_SINK}.monitor → ${LISTEN_SINK}' already exists."
else
    pactl load-module module-loopback \
        source="${REMOTE_SINK}.monitor" \
        sink="${LISTEN_SINK}" \
        latency_msec=50 >/dev/null
    echo "Created loopback '${REMOTE_SINK}.monitor → ${LISTEN_SINK}'."
fi

cat <<EOF

Joya audio routing is ready.

In your call app (Discord/Zoom/…):
  - Output device → "Call_Remote"      (the other party goes to call_remote)
  - Microphone    → "Monitor of Joya_Mic"

In Joya's config.yaml:
  audio:
    relay:
      capture_device: ${REMOTE_SINK}
    output:
      mic_sink: ${MIC_SINK}

Run \`cargo run -- list-devices\` to confirm the exact capture name/id. The
isolation sink's monitor is listed under Input devices as "Call_Remote" with
id: ${REMOTE_SINK} (cpal exposes the monitor under the sink's node name, without
a .monitor suffix).
Tip: set JOYA_LISTEN_SINK=<sink> to hear the other party on a non-default output.
EOF
