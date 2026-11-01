# Media Organizer

C 드라이브와 여러 외장 드라이브에 흩어진 사진·동영상을 **한 앱에서** 찾고, 미리보고, 라벨을 붙이고, 중복을 지우고, 한 폴더로 날짜별로 모으는 Windows 데스크톱 앱입니다. Docker·Python·exiftool 같은 다른 프로그램 설치 없이 설치 파일 하나로 동작합니다.

## 할 수 있는 것

| 기능 | 내용 |
|---|---|
| 사용자 파일만 찾기 | 시스템·프로그램·캐시 폴더, 게임·앱 설치 폴더 안의 이미지, 숨김·시스템 파일, OneDrive 온라인 전용 파일, 아이콘 크기 이미지를 건너뜀. 무엇을 왜 건너뛰었는지 스캔 결과에 표시 |
| 내 폴더 빠르게 추가 | 사진·동영상·바탕 화면·다운로드·OneDrive 폴더와 연결된 외장 드라이브를 체크해서 한 번에 추가 |
| 정렬 | 촬영일, 수정일, 파일 크기, 해상도, 동영상 길이, 이름. 정렬 기준에 맞춰 묶어서 표시 (예: "1GB 이상", "12–40MP") |
| 검색 조건 | 기간(최근 30일·올해·작년·연도·직접 지정), 크기, 해상도, 동영상 길이, 카메라, 파일 형식, 라벨, 드라이브, 날짜 신뢰도, 위치정보. 프리셋 한 번 클릭으로 적용하고 칩으로 하나씩 해제 |
| 여러 드라이브 통합 보기 | 폴더(드라이브 전체도 가능)를 추가하면 월별 타임라인 하나에 모두 표시. 외장 드라이브를 빼도 썸네일과 정보가 남아 계속 볼 수 있음 |
| 드라이브 인식 | 드라이브 문자(E:, F:)가 바뀌어도 볼륨 시리얼 번호로 같은 디스크로 인식 |
| 미리보기 | 사진은 원본, HEIC·RAW·TIFF는 변환 미리보기, 동영상은 앱 안에서 재생. HEVC·AVI·MTS처럼 바로 재생이 안 되는 형식은 자동 변환 |
| 날짜 판별 | EXIF → 파일 이름(IMG_20190512, KakaoTalk_…, PXL_…, WhatsApp) → 폴더 이름(2019-05 제주도) → 파일 수정일 순서. 출처와 신뢰도를 표시하고 직접 수정 가능 |
| 자동 라벨 | 스크린샷, 카카오톡·WhatsApp·텔레그램·LINE·인스타그램·다운로드, 카메라 기종, 위치정보, 편집본, 날짜 불확실 |
| 사용자 라벨 | 여러 장 선택 후 일괄 라벨·날짜 지정, 라벨로 필터 |
| 중복 정리 | 완전히 같은 파일(BLAKE3 해시), 비슷한 사진(지각 해시), 비슷한 동영상. 그룹마다 가장 좋은 파일을 자동으로 "보존" 추천 |
| 한 곳으로 정리 | `{year}/{year}-{month}`, `{kind}/{year}` 등 템플릿으로 이동 또는 복사. 먼저 미리보기(디스크 변경 없음)로 확인 후 실행 |
| 되돌리기 | 모든 이동·복사·삭제는 기록되어 되돌릴 수 있음 |

## 건너뛰는 파일 (사용자 파일만 찾기)

「드라이브 · 폴더 → 스캔 설정」에서 모두 바꿀 수 있습니다. 바꾼 뒤에는 「전체 다시 스캔」을 해야 적용됩니다.

| 규칙 | 기본값 | 예 |
|---|---|---|
| 건너뛸 폴더 이름 (어느 위치든) | Windows, Program Files, ProgramData, AppData, $Recycle.Bin, System Volume Information, steamapps, node_modules, cache, temp, .thumbnails 등 | C:\Windows\Web\Wallpaper, AppData의 앱 캐시 |
| Windows 프로필 템플릿 | Users\Default, Users\All Users | |
| 숨김·시스템 속성 | 켜짐 | DCIM\.thumbnails, 숨김 폴더 |
| 프로그램·게임 설치 폴더 | 켜짐 | DLL·PAK가 3개 이상이거나 uninstall 파일이 있는 폴더와 그 하위 폴더 |
| OneDrive 온라인 전용 | 켜짐 | 읽기만 해도 다운로드가 시작되는 파일 |
| 작은 이미지 | 10KB 미만 또는 긴 변 400px 미만 | 아이콘, 이모티콘, 웹 썸네일 |

심볼릭 링크와 NTFS 정션은 따라가지 않습니다. 그래서 Windows 프로필의 "Application Data" 같은 반복 링크에 빠지지 않습니다.

## 안전 설계

- **미리보기는 아무것도 바꾸지 않습니다.** 정리 계획은 폴더도 만들지 않습니다.
- **삭제는 격리입니다.** 중복 정리로 지운 파일은 같은 드라이브의 격리 폴더로 이름만 바뀌어 이동합니다. 시스템 드라이브는 `%USERPROFILE%\.media-organizer\quarantine`, 외장 드라이브는 `E:\.media-organizer-quarantine`입니다. 휴지통이 없는 USB(FAT32/exFAT)에서도 되돌릴 수 있습니다. 「격리함 비우기」만이 영구 삭제입니다.
- **다른 드라이브로의 이동**은 복사 → BLAKE3로 내용 검증 → 원본 삭제 순서이며, 임시 이름으로 복사한 뒤 바꾸므로 중간에 꺼져도 반쪽 파일이 남지 않습니다.
- **이름 충돌**: 대상에 같은 내용의 파일이 이미 있으면 건너뛰고(중복 도구에 맡김), 내용이 다르면 `_1`, `_2`를 붙입니다.
- **비슷한 사진·영상은 자동 적용을 권하지 않습니다.** 앱이 확인을 요청합니다.

## 무엇을 가져다 썼나

처음부터 만들지 않고, 검증된 오픈소스 부품을 조립했습니다.

| 부품 | 역할 | 라이선스 |
|---|---|---|
| [Tauri 2](https://tauri.app) | 데스크톱 셸, 설치 파일(MSI/NSIS), WebView2 | MIT/Apache-2.0 |
| [czkawka_core](https://github.com/qarmin/czkawka) 12 | 비슷한 사진·동영상 탐지 | MIT |
| [nom-exif](https://github.com/mindeng/nom-exif) | 사진·동영상 메타데이터 (exiftool 불필요) | MIT |
| [image](https://github.com/image-rs/image), imagesize | 썸네일 디코딩 | MIT/Apache-2.0 |
| rusqlite (SQLite 내장) | 색인 DB | MIT |
| blake3 | 파일 해시 | CC0/Apache-2.0 |
| Windows Shell 썸네일러 | HEIC·RAW·동영상 썸네일 (OS 내장 코덱) | — |
| ffmpeg (설치 파일에 포함) | 동영상 썸네일·변환, 유사 영상 | LGPL 빌드 기본 |
| [phockup](https://github.com/ivandokov/phockup) | 날짜 폴더 정리·충돌 처리 로직을 참고해 포팅 | MIT |
| React, react-virtuoso | UI, 50만 장 가상 스크롤 | MIT |

## 구조

```
crates/media-core/      UI와 무관한 모든 로직 (Linux/Windows에서 헤드리스 테스트 가능)
  volumes.rs            볼륨 시리얼 기반 드라이브 식별, 온라인/오프라인, 격리 폴더 위치
  userfiles.rs          사용자 파일 판별 규칙 (시스템·앱·숨김·클라우드·아이콘 건너뛰기), 스캔 설정 저장
  scanner.rs            증분 스캔 (크기+수정시각 변경분만 재처리), 병렬 메타데이터 추출
  meta.rs               EXIF/동영상 메타데이터, 날짜 추론 체인
  labels.rs             자동/사용자 라벨, 날짜 수정
  thumbs.rs             썸네일·미리보기 캐시, 디코드 체인 (image → Windows Shell → ffmpeg)
  dedupe.rs             완전 중복(자체), 유사 사진·영상(czkawka_core), 보존 추천, 적용
  organizer.rs          템플릿, 계획(드라이런), 실행
  journal.rs            격리/이동/복사 기록, 되돌리기, 격리함 비우기
  tests/e2e.rs          가상 드라이브 3개로 전체 흐름 검증
src-tauri/              Tauri 명령 계층, thumb:// 프로토콜, 작업 진행률 이벤트
src/                    React UI (라이브러리, 뷰어, 드라이브, 중복, 정리, 작업 기록)
scripts/                Windows 빌드·ffmpeg 다운로드 스크립트
```

데이터 위치:

- 색인 DB: `%APPDATA%\com.mediaorganizer.app\library.db`
- 썸네일 캐시: `%LOCALAPPDATA%\com.mediaorganizer.app\cache`. 썸네일 한 장은 약 8~25KB라서, 50만 장이면 4~12GB 정도입니다.

## Windows에서 빌드하기

필요한 것은 한 번만 설치합니다.

1. [Rust](https://rustup.rs). 기본 MSVC 툴체인을 사용합니다.
2. [Node.js 20+](https://nodejs.org)
3. [Visual Studio Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/)의 "C++ 데스크톱 개발" 워크로드

PowerShell에서 실행합니다.

```powershell
.\scripts\build-windows.ps1
```

`target\release\bundle\msi\*.msi`와 `target\release\bundle\nsis\*-setup.exe`가 생성됩니다. 스크립트는 ffmpeg/ffprobe를 받아 `src-tauri\binaries\`에 두고 설치 파일에 포함합니다.

개발 모드(핫 리로드):

```powershell
npm ci
.\scripts\fetch-ffmpeg.ps1
npx tauri dev
```

## 테스트

```bash
cargo test -p media-core
```

e2e 테스트는 가상 드라이브 3개에 EXIF가 있는 JPEG, 메신저 파일, 스크린샷, 완전 중복, 크기만 다른 사본, 이름 충돌을 만듭니다. Program Files·AppData·게임 폴더·숨김 폴더 안의 사진과 300px 스티커처럼 건너뛰어야 할 파일도 함께 넣고 다음 흐름을 검증합니다. 마지막에는 모든 파일이 바이트 단위로 원래 위치에 돌아왔는지 확인합니다.

1. 스캔과 건너뛰기 규칙
2. 날짜·라벨 판별, 정렬·범위 검색
3. 썸네일 생성
4. 중복 격리와 되돌리기
5. 유사 사진 그룹
6. 다른 드라이브로 정리
7. 재스캔
8. 정리 되돌리기

수동 확인용 데모 데이터:

```bash
cargo run -p media-core --example demo_fixture -- ./demo
cargo run -p media-core --example demo_index -- <data_dir> <cache_dir> ./demo/diskC ./demo/usbA ./demo/usbB
```

## 다음 단계 (계획)

- **CLIP 내용 라벨·자연어 검색**: `ort`(ONNX Runtime)와 CLIP ViT-B/32를 사용합니다. 음식, 풍경, 문서, 반려동물 같은 라벨이 `labels` 테이블에 `source='ml'`로 추가됩니다.
- **얼굴 인식·인물 묶기**: InsightFace ONNX를 사용합니다.
- **썸네일 캐시 위치 설정**: 지금은 `%LOCALAPPDATA%` 고정입니다.
- **코드 서명**: 서명하지 않은 설치 파일은 SmartScreen 경고가 뜹니다.
