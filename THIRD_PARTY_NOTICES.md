# Third-party notices

Media Organizer is built from these open-source projects and data. Full license texts ship with
the app (installer: `ai\` folder for the AI runtime; the rest in each project's repository).

| Component | Use in the app | License |
|---|---|---|
| [Tauri](https://tauri.app) | App shell, installer, auto-update | MIT / Apache-2.0 |
| [czkawka_core](https://github.com/qarmin/czkawka) | Similar photos and videos | MIT |
| [nom-exif](https://github.com/mindeng/nom-exif) | Photo / video metadata | MIT |
| [image](https://github.com/image-rs/image), jpeg-decoder, imagesize | Decoding, thumbnails | MIT / Apache-2.0 |
| [rusqlite](https://github.com/rusqlite/rusqlite) / [SQLite](https://sqlite.org) | Index database | MIT / Public Domain |
| [ort](https://github.com/pykeio/ort) | Calling ONNX Runtime from Rust | MIT / Apache-2.0 |
| [ONNX Runtime](https://github.com/microsoft/onnxruntime) 1.24.4 (DirectML build) | Running the face models on the PC | MIT — `ai\ONNXRUNTIME-LICENSE.txt`, `ai\ONNXRUNTIME-ThirdPartyNotices.txt` |
| DirectML (`DirectML.dll`) | GPU acceleration for the face models | Microsoft DirectML license, redistributable inside applications built with ML frameworks — `ai\DirectML-LICENSE.txt` |
| [YuNet](https://github.com/opencv/opencv_zoo/tree/main/models/face_detection_yunet) (OpenCV Zoo) | Finding faces | MIT |
| [SFace](https://github.com/opencv/opencv_zoo/tree/main/models/face_recognition_sface) (OpenCV Zoo) | Comparing faces | Apache-2.0 |
| [reverse_geocoder](https://github.com/gx0r/rrgeo) with [GeoNames](https://www.geonames.org) data | GPS to place names, trips | MIT / Apache-2.0; data CC-BY 4.0 (geonames.org) |
| [FFmpeg](https://ffmpeg.org) (BtbN LGPL build) | Video thumbnails and conversion | LGPL |
| [React](https://react.dev), [react-virtuoso](https://virtuoso.dev) | User interface | MIT |

All processing happens on the user's PC; no photo, face or location data leaves it.
