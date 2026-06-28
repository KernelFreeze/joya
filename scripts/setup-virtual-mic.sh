#!/usr/bin/env bash
# Create a virtual microphone for Joya.
#
# Joya plays the synthesized translation into a null sink; that sink's *monitor*
# becomes a microphone you can select in your call/conference app. Run this once
# per session (modules are not persistent across reboots).
#
# Usage:
#   scripts/setup-virtual-mic.sh           # load the null sink
#   scripts/setup-virtual-mic.sh teardown  # unload it
#
# After loading, set `audio.mic_sink: joya_mic` in Joya's config.yaml and pick
# "Joya Mic" (the monitor) as the microphone in your call app.

set -euo pipefail

SINK_NAME="joya_mic"

if [[ "${1:-}" == "teardown" ]]; then
    pactl list short modules \
        | awk -v s="sink_name=${SINK_NAME}" '$0 ~ s {print $1}' \
        | while read -r id; do pactl unload-module "$id"; done
    echo "Removed virtual mic '${SINK_NAME}'."
    exit 0
fi

if pactl list short sinks | grep -q "${SINK_NAME}"; then
    echo "Virtual mic '${SINK_NAME}' already exists."
    exit 0
fi

pactl load-module module-null-sink \
    sink_name="${SINK_NAME}" \
    sink_properties=device.description="Joya_Mic" >/dev/null

echo "Created virtual mic '${SINK_NAME}'."
echo "  - Set 'audio.mic_sink: ${SINK_NAME}' in Joya's config.yaml"
echo "  - Select 'Monitor of Joya_Mic' as the microphone in your call app"
