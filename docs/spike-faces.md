# Spike: on-device faces (2026-11-04)

Result: **go**. Model: **SFace** (OpenCV Zoo, Apache-2.0) with **YuNet** detector (OpenCV Zoo, MIT —
both licenses read from the upstream LICENSE files). AuraFace could not be evaluated here (Hugging Face
unreachable from the build environment); `faces.model` keeps the door open for a later swap.

Runtime: ONNX Runtime **1.24.4** loaded at run time (`ort` 2.0.0-rc.15, `load-dynamic`, `api-24`).
1.24.4 is the newest ONNX Runtime with a DirectML build (PyPI `onnxruntime-directml`), so the API is
pinned to 24. Linux runs the CPU build for tests; Windows cross-check with `directml` compiles.

## Accuracy (cosine similarity of SFace embeddings)

| Pair | Similarity |
|---|---|
| Same person, different photos (Obama ×3, Biden ×2) | 0.725 – 0.976 |
| Different people | 0.073 – 0.272 |
| Landscapes / screenshot | 0 faces detected (no false positives) |
| 427×240 frame | 55 px face detected |

OpenCV's documented same-person threshold is 0.363. Chosen defaults: link faces ≥ 0.42, assign to an
existing person ≥ 0.40, "maybe this person?" suggestions 0.30 – 0.40.

## Speed (CPU, 4 threads, release build)

| Step | Time |
|---|---|
| Load both models | ~0.1 s (once) |
| YuNet detection (640×640) | 9 – 24 ms per photo |
| SFace embedding | 13 – 26 ms per face |
| 12 MP JPEG full decode (image crate) | ~57 ms |
| 12 MP JPEG DCT-scaled decode to 1008×756 (jpeg-decoder) | ~35 ms (1.6× faster, not 4–8× as first estimated) |

Rough total per photo on CPU: ~100 ms + disk read → about 3 hours for 100k photos on one core group;
DirectML speed must be measured on a real Windows PC.

Test images: `ageitgey/face_recognition` examples (official photos of public officials), used only
locally for this measurement.
