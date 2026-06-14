# minisync

같은 네트워크의 컴퓨터들이 **폴더를 평등하게(서버 없이) 공유·관리**하는 P2P 동기화 도구.
모든 노드가 직접 연결되는 full‑mesh, TLS 암호화, CRDT 텍스트 병합. Rust 단일 바이너리.
**macOS · Windows · Linux** 모두 GUI를 지원합니다.

## 설계 컨셉 (3원칙)

1. **평등한 P2P** — 중앙 서버/마스터 없음. 모든 노드가 동등한 공동 소유자.
2. **소유 명확성** — *누가 무엇을 가졌는지* 항상 알 수 있다 (원본 + 보유자).
3. **이력 관리** — *누가 언제 무엇을 바꿨는지* 변경 이력이 노드 간 공유된다.

---

## 다운로드 (Pre-built)

[**GitHub Releases**](https://github.com/omnyx2/MiniSync/releases)에서 받으세요:

| OS | 파일 | 설치 |
|---|---|---|
| macOS (Apple Silicon) | `minisync-macos-arm64.dmg` | `.dmg` 열고 **minisync.app** 꺼내서 실행 |
| Windows (x64) | `minisync-windows-x64.zip` | 압축 풀고 `minisync-windows-x64.exe` 더블클릭 |
| Linux (x64, glibc) | `minisync-linux-x64.tar.gz` | 풀고 `chmod +x minisync` 후 실행 |

> **첫 실행 보안 경고 (아직 코드서명 안 됨):**
> macOS — `minisync.app` 우클릭 → **열기** (또는 `xattr -dr com.apple.quarantine minisync.app`) ·
> Windows — SmartScreen **추가 정보 → 실행** ·
> Linux — 데스크톱 세션 + `libGL`/`libxkbcommon` 필요, 한글 파일명은 Noto/Nanum CJK 폰트 설치 시 표시.

---

## 실행 방법

```
minisync [--gui] <동기화폴더> <내_주소> [상대방_주소 ...]
```

| 인자 | 설명 | 예시 |
|---|---|---|
| `<동기화폴더>` | 동기화할 폴더 (없으면 생성) | `~/Sync` |
| `<내_주소>` | 수신 IP:포트 | `0.0.0.0:9000` |
| `[상대방_주소]` | 연결할 노드들 (같은 LAN이면 생략 가능 — 자동 발견) | `192.168.1.100:9000` |
| `--gui` | 데스크톱 GUI (**3 플랫폼 모두**) | |

- **GUI**: 앱을 더블클릭하거나 `--gui`로 실행. 인자 없이 실행하면 저장된 설정/기본 폴더로 GUI가 열립니다.
- **헤드리스**: `--gui`를 빼면 창 없이 백그라운드 동기화 (서버 등).
- 한 번 실행하면 설정이 저장되어, 다음부터는 인자 없이 실행 가능.

```bash
# 2대 예시 (Mac + Ubuntu)
./minisync --gui ~/Sync 0.0.0.0:9000 192.168.1.100:9000   # Mac
./minisync       ~/Sync 0.0.0.0:9000 192.168.1.50:9000    # Ubuntu (헤드리스)
```

---

## GUI 둘러보기

파일 브라우저는 폴더 탐색형이며, 각 파일 행에 다음 열이 있습니다:

| 열 | 의미 |
|---|---|
| **Name** | 파일/폴더 (폴더 클릭 시 진입) |
| **Size** | 크기 |
| **Location** | **원본(origin) + 보유자 수** — 클릭하면 보유자 목록 드롭다운. 원본이 사본을 안 가지면 `(no copy)` |
| **Available** | `✓ here`(내가 보유) / `● online`(보유자 온라인 → 받기 가능) / `unavailable`(보유자 전원 오프라인) |
| **Sync** | 토글 — `auto-sync`(이 기기에 보관, 자동 갱신) ↔ `off`(참조만). 텍스트는 항상 `auto-sync`(잠김) |
| **Delete** | 🗑 **전체 삭제** (모든 기기에서 삭제, 확인창) |

- **폴더 행**: `N/M`(하위 동기화 개수)와 우측 **⬇ Sync all** 버튼(하위 전체 다운로드 — 진행률 창 + 취소; 보유자 오프라인 파일은 자동 skip).
- **History** 버튼: 누가 언제 무엇을 추가/수정/삭제했는지 이력.
- **드래그&드롭**으로 파일 임포트, **동시 편집 충돌** 시 상단 경고 배너.

---

## 동기화 동작 ("무엇이 최신인가")

파일 종류에 따라 다르게 처리합니다:

- **텍스트/코드** (`.md` `.txt` `.json` `.csv` 소스 등): **CRDT(Automerge) 병합** — "최신"이라는 개념 없이
  변경이 **합쳐집니다**. 동시 편집·오프라인 편집도 손실 없이 병합.
- **바이너리** (이미지/PDF/zip/문서 등): **버전 벡터(인과 이력)** — 한쪽이 더 최신이면 그것으로 교체,
  **독립 동시 편집이면 충돌로 보고 `파일.conflict-<peer>` 사본을 둘 다 보존**(조용한 손실 없음),
  비길 때만 수정 시각(mtime). 껐을 때 한 편집(오프라인)도 시작 시 감지해 인과를 추적합니다.

---

## 선택적 동기화 (Selective Sync)

기본적으로 파일은 모든 노드에 자동 복사되지 않고 **참조(메타데이터)만 공유**됩니다.
각 기기는 **Sync 토글로 고른 파일만** 실제로 보관합니다.

- **auto-sync 켬** = 이 기기에 사본 보관 + 자동 갱신
- **off** = 참조만 (필요할 때 다시 켜서 받기)
- **Remove(off)는 네트워크 삭제가 아닙니다** — 내 로컬 캐시만 비웁니다. 다른 노드 원본은 그대로.
- 전체 네트워크에서 지우려면 **Delete** 열(확인 거침).
- 텍스트/코드는 작고 병합 이점이 있어 **항상 동기화**(CRDT).

폴더별 규칙은 `<폴더>/.minisync/config.toml`로 지정 (아래 [설정](#설정) 참고).

---

## 주요 기능

- **평등 P2P full‑mesh** — 중앙 서버 없이 노드끼리 직접 연결
- **TLS 암호화** — 자체서명 인증서로 전 통신 암호화
- **CRDT 텍스트 병합** + **버전 벡터 충돌 감지**(conflict 사본)
- **선택적 동기화** — 참조 공유 + 고른 파일만 보관
- **원본/보유자 추적 + Available** — 누가 가졌는지, 지금 받을 수 있는지
- **변경 이력 공유** — 누가/언제/무엇을 (모든 노드가 같은 이력으로 수렴)
- **실시간 피어 추적** — heartbeat(4초 ping / 12초 timeout)로 죽은 피어 빠르게 감지
- **Sync all** — 폴더 하위 전체 다운로드(진행률·취소·재개)
- **오프라인 편집 감지** — 껐을 때 바뀐 파일도 시작 시 인과 추적
- **LAN 자동 발견** (UDP) · **lattice VPN 오버레이**(`--lattice`)
- **GUI** (macOS/Windows/Linux): 파일 목록·피어·이력·드래그&드롭

---

## 설정

### 전역 설정 (`~/.config/minisync/app.toml`)
```toml
sync_folder = "/home/user/Sync"
listen_addr = "0.0.0.0:9000"
peers = ["192.168.1.100:9000"]
node_name = "my-laptop"      # GUI 표시 이름 (기본: 호스트명)
```

### 폴더별 규칙 (`<동기화폴더>/.minisync/config.toml`)
```toml
default_mode = "reference"   # 또는 "full_copy"

[[rules]]
pattern = "*.pdf"
mode = "reference"           # 메타데이터만, 필요시 다운로드

[[rules]]
pattern = "src/**"
mode = "full_copy"           # 항상 전체 복사
```

---

## 네트워크

- 수신 포트(예: 9000)가 방화벽에서 열려 있어야 함.
- 같은 LAN: 사설 IP로 바로 연결되며, **상대 주소 없이도 UDP 비콘(포트 19531)으로 자동 발견**.
- 인터넷 너머: 포트포워딩 또는 Tailscale/WireGuard/lattice 등 VPN.

### lattice VPN 오버레이 (`--lattice`)
[lattice](https://github.com/omnyx2) 메시 VPN 위에서 NAT 너머 동기화:
```bash
minisync --lattice ~/Sync 0.0.0.0:9000
```
- 모든 노드가 **같은 수신 포트**를 사용해야 함. lattice 데몬 실행 중 + 실행 파일 이름이 `minisync`여야 함.
- `--lattice` 모드에선 LAN UDP 자동 발견이 꺼집니다.

---

## 소스에서 빌드

```bash
# Rust 설치: https://rustup.rs
cargo build --release                 # 헤드리스
cargo build --release --features gui  # GUI 포함

# Linux 크로스 컴파일 (macOS에서, 헤드리스):
#   cargo install cargo-zigbuild && rustup target add x86_64-unknown-linux-musl
cargo zigbuild --release --target x86_64-unknown-linux-musl
```
결과물: `target/release/minisync`. (GUI 리눅스 배포 바이너리는 해당 리눅스에서 빌드하세요.)

### Docker로 노드 띄우기 (개발용)
`Dockerfile` / `docker-compose.yml` / `docker-sync.sh` 참고 — 컨테이너로 노드를 실행하거나
호스트↔컨테이너 폴더를 동기화하는 예시.

---

자세한 변경 내역은 [CHANGELOG.md](CHANGELOG.md) 참고.
