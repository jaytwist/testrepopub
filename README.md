# Media Organizer

C 드라이브와 여러 외장 드라이브에 흩어진 사진·동영상을 **한 앱에서** 찾고, 미리보고, 라벨을 붙이고, 중복을 지우고, 한 폴더로 날짜별로 모으는 Windows 데스크톱 앱입니다. Docker·Python·exiftool 같은 다른 프로그램 설치 없이 설치 파일 하나로 동작합니다.

## 할 수 있는 것

| 기능 | 내용 |
|---|---|
| 사용자 파일만 찾기 | 시스템·프로그램·캐시 폴더, 게임·앱 설치 폴더 안의 이미지, 숨김·시스템 파일, OneDrive 온라인 전용 파일, 아이콘 크기 이미지를 건너뜀. 무엇을 왜 건너뛰었는지 스캔 결과에 표시 |
| 내 폴더 빠르게 추가 | 사진·동영상·바탕 화면·다운로드·OneDrive 폴더와 연결된 외장 드라이브를 체크해서 한 번에 추가 |
| 정렬 | 촬영일, 수정일, 파일 크기, 해상도, 동영상 길이, 이름. 정렬 기준에 맞춰 묶어서 표시 (예: "1GB 이상", "12–40MP") |
| 검색 조건 | 기간(최근 30일·올해·작년·연도·직접 지정), 크기, 해상도, 동영상 길이, 카메라, 파일 형식, 라벨, 드라이브, 날짜 신뢰도, 위치정보. 프리셋 한 번 클릭으로 적용하고 칩으로 하나씩 해제 |
| 인물 | 같은 사람이 나온 사진을 자동으로 묶음(YuNet + SFace, PC 안에서만). 카드에서 바로 이름 붙이기 — 같은 이름이면 합치기 제안, 「같은 사람인가요?」(같음/다름/모르겠음), 「이 사람 아님」, 숨기기. 검색창에 이름, 뷰어에 인물 표시. 설정에서 끄기·얼굴 데이터 지우기 |
| 다시 읽지 않기 | 한 번 처리한 사진은 다시 읽지 않음: 앱 밖에서 옮기거나 이름을 바꾼 파일은 내용(크기·시각·앞 64KB)으로 알아보고 분석·인물·라벨·날짜 수정을 그대로 유지. 백업 드라이브의 복사본은 기존 결과를 재사용. 얼굴도 첫 분석과 같은 한 번의 읽기에서 처리 |
| 여행 | 사진의 GPS와 날짜로 집(해마다 사진을 가장 많은 날 찍은 도시)을 알아내고, 80km 넘게 떠난 기간을 "2024년 6월 Hawaii 여행"처럼 자동으로 묶음. 검색창에 도시 이름을 쳐도 찾음, 뷰어에 장소 표시, 정리 폴더 `{year}/{trip}` |
| 시간 막대 | 월별·연도별로 볼 때 오른쪽에 Google Photos처럼 연도 눈금과 월 표시. 마우스를 올리면 "2024년 6월", 누르거나 끌면 그 위치로 바로 이동(큰 달의 중간도), 막대 위에서 휠도 동작, 스크롤하는 동안 지금 연·월 표시 |
| 비교 | 라이브러리에서 2–4장을 골라 「⇆ 비교」(중복 정리 화면의 각 묶음에도): 나란히 크게 보기, 누르면 모두 같은 곳 확대, 폴더·드라이브·촬영 날짜·수정 시각·크기·해상도 등을 줄 맞춰 비교(다른 값은 진하게), "내용 완전히 같음" 판정, ★ 추천 1장 남기고 나머지 삭제(되돌리기 가능) |
| 드라이브별로 보기 | 라이브러리 위의 탭으로 「모든 드라이브」를 합쳐 보거나 드라이브 하나만 따로 봄 |
| 빠른 다시 스캔 | 「새 파일 확인」은 크기·수정 시각이 바뀐 파일만 다시 읽음. 중복 찾기도 저장된 지문을 재사용해 크기가 겹치는 새 파일만 읽음 |
| 정리 제안 | 연사(가장 선명한 한 장만 남기고 나머지 미리 선택), 흐린 사진(같은 날 사진과 비교해 판단 — 배경만 흐린 인물 사진·하늘은 제외), 스크린샷, 500MB 이상 동영상, 똑같은 사본을 한 화면에. 「한 장씩 검토」: ← → 이동, K 남기기(다시 제안 안 함), Delete 삭제 목록에 담기, Esc로 끝내면 담은 파일을 한 번에 격리. 라이브러리의 「연사 겹쳐 보기」로 연사를 한 장 + "+N"으로 표시 |
| 선택한 파일 삭제 | 선택 막대의 「🗑 삭제」나 Delete 키. 개수와 용량을 확인한 뒤 같은 드라이브의 격리 폴더로 옮기고, 바로 「되돌리기」 가능. 「작업 기록 → 격리함 비우기」로 완전 삭제 |
| 묶어 보기 · 묶음 선택 · 접기 | 월별·연도별·폴더별로 묶고(정렬과 별개), 묶음 제목 옆 동그라미로 묶음 전체 선택, ▾로 접기, 「모두 접기/펼치기」. 접힌 상태는 기억됨. 50만 장에서도 묶음 단위로 필요한 만큼만 불러옴 |
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

## 자동 업데이트

설치된 앱은 켤 때 새 버전을 조용히 확인합니다. 새 버전이 있으면 왼쪽에 「새 버전 vX.Y.Z · 업데이트 후 재시작」 카드가 뜹니다. 누르면 내려받기, 서명 확인, 설치, 재시작이 자동으로 진행됩니다. 스캔이나 정리 작업 중에는 업데이트하지 않고, 서명이 맞지 않는 파일은 설치하지 않습니다. 색인, 썸네일, 라벨은 AppData에 있어서 그대로 남습니다.

- 업데이트 주소: `https://github.com/jaytwist/testrepopub/releases/latest/download/latest.json` (`src-tauri/tauri.conf.json`의 `plugins.updater.endpoints`)
- 업데이트는 **설치 파일(NSIS/MSI)로 설치한 앱**에서만 동작합니다. 휴대용 zip 버전은 직접 덮어써야 합니다.
- 저장소가 **공개(public)** 여야 앱이 로그인 없이 업데이트를 받을 수 있습니다. 비공개 저장소를 원하면 릴리스 파일만 올리는 공개 저장소를 따로 두면 됩니다.

### 처음 한 번 설정

1. Claude GitHub 앱이 이 저장소에 접근할 수 있게 합니다 (https://claude.ai/connect-github).
2. 저장소 Settings → Secrets and variables → Actions → New repository secret
   - 이름: `TAURI_SIGNING_PRIVATE_KEY`
   - 값: 업데이트 서명 개인 키 파일(`updater.key`)의 내용 전체
3. 개인 키는 안전한 곳에 백업합니다. 잃어버리면 이미 설치된 앱에 더 이상 업데이트를 보낼 수 없습니다. git에는 절대 올리지 않습니다(`.secrets/`는 git에서 제외됨).

### 새 버전 배포하기

```bash
npm run release -- 0.2.0 "바뀐 점을 여기에 적습니다"
```

버전 번호를 package.json, tauri.conf.json, Cargo.toml에서 한꺼번에 올리고, 커밋과 태그를 만들어 push합니다. GitHub Actions가 서명된 Windows 설치 파일과 `latest.json`을 만들어 Releases에 올리는 데 약 10~15분 걸립니다. 그 뒤로 설치된 앱들이 업데이트를 알립니다. 적은 "바뀐 점"이 앱의 업데이트 카드에 그대로 표시됩니다.

## VLC 연결

VLC가 설치되어 있으면 앱이 자동으로 찾습니다. 찾는 위치는 Program Files, 사용자별 설치 경로, PATH입니다. 뷰어에 「VLC로 재생」 버튼이 생기고, 앱 안에서 바로 재생되지 않는 형식(HEVC, MTS, AVI 등)은 변환 대신 VLC로 여는 것을 먼저 제안합니다.

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

- **장소 · 여행** (v0.2.0): GPS → 지명(`reverse_geocoder`, GeoNames), 연도별 집 위치, 여행 자동 묶기.
- **썸네일 캐시 위치 설정**: 지금은 `%LOCALAPPDATA%` 고정입니다.
- **코드 서명**: 서명하지 않은 설치 파일은 SmartScreen 경고가 뜹니다. 업데이트 서명과는 별개입니다.
