#!/usr/bin/env bash

if [[ $# -lt 1 || $# -gt 3 ]]; then
    echo "Usage: $0 <MEDIA URL> [RESOLUTION_PX] [AUDIO_BITRATE_KBPS]"
    exit 1
fi

MEDIA_URL="$1"
RESOLUTION="${2:-720}"
OUTPUT_FILE="output.mp4"

REQUEST_ARGS=(-d "url=$MEDIA_URL" -d "res=$RESOLUTION")

if [[ $# -ge 3 ]]; then
    REQUEST_ARGS+=(-d "abr=$3")
fi

curl -X POST \
     "${REQUEST_ARGS[@]}" \
     -o "$OUTPUT_FILE" \
     http://127.0.0.1:3000/download
