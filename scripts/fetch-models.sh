#!/usr/bin/env bash
# Fetch the face-recognition models Solar bundles. They are NOT committed to git
# (37 MB), so run this once after cloning, before `npm run tauri dev|build`.
#
#   YuNet  — face detection   — MIT License (Shiqi Yu)
#   SFace  — face embedding    — Apache-2.0 License
# Both from the OpenCV Zoo (https://github.com/opencv/opencv_zoo).
set -euo pipefail

DIR="$(cd "$(dirname "$0")/.." && pwd)/src-tauri/models"
mkdir -p "$DIR"

fetch() {
  local name="$1" url="$2"
  if [ -f "$DIR/$name" ]; then
    echo "✓ $name already present"
  else
    echo "↓ $name"
    curl -fL --retry 3 -o "$DIR/$name" "$url"
  fi
}

base="https://github.com/opencv/opencv_zoo/raw/main/models"
fetch yunet.onnx "$base/face_detection_yunet/face_detection_yunet_2023mar.onnx"
fetch sface.onnx "$base/face_recognition_sface/face_recognition_sface_2021dec.onnx"

echo "Done. Models in $DIR"
