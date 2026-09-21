#!/usr/bin/env bash

if [[ $# -ne 1 ]]; then
    echo "Usage: $0 <MEDIA URL>"
    exit 1
fi

MEDIA_URL="$1"
OUTPUT_FILE="output.mp4"

curl -X POST \
     -d "url=$MEDIA_URL" \
     -o "$OUTPUT_FILE" \
     http://127.0.0.1:3000/download
