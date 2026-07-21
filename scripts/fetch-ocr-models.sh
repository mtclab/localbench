#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
models_dir="$repo_root/app-ocr/public/models"
mkdir -p "$models_dir"

fetch_model() {
  local name=$1
  local url=$2
  local minimum_bytes=$3
  local destination="$models_dir/$name"
  local temporary
  temporary=$(mktemp "$models_dir/.${name}.XXXXXX")

  if ! curl \
    --fail \
    --location \
    --proto '=https' \
    --retry 3 \
    --show-error \
    --silent \
    --tlsv1.2 \
    --output "$temporary" \
    "$url"; then
    rm -f -- "$temporary"
    return 1
  fi

  local downloaded_bytes
  downloaded_bytes=$(wc -c < "$temporary")
  if (( downloaded_bytes < minimum_bytes )); then
    rm -f -- "$temporary"
    echo "Refusing unexpectedly small $name download ($downloaded_bytes bytes)." >&2
    return 1
  fi

  chmod 0644 "$temporary"
  mv -f -- "$temporary" "$destination"
  echo "Fetched $name ($downloaded_bytes bytes)."
}

fetch_model \
  "text-detection.rten" \
  "https://ocrs-models.s3-accelerate.amazonaws.com/text-detection.rten" \
  2000000
fetch_model \
  "text-recognition.rten" \
  "https://ocrs-models.s3-accelerate.amazonaws.com/text-recognition.rten" \
  9000000
