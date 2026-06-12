# minisync

P2P 폴더 동기화 도구. Full mesh 구조로 모든 노드가 직접 연결되며, TLS 암호화 통신.
Rust 단일 바이너리로 별도 의존성 없이 실행 가능.

## 빌드 없이 바로 사용 (Pre-built Binaries)

[**GitHub Releases**](https://github.com/omnyx2/MiniSync/releases)에서 플랫폼별 바이너리를 받으세요:

| 파일 | 플랫폼 | 아키텍처 | 비고 |
|---|---|---|---|
| `minisync-macos` | macOS | Apple Silicon (arm64) | GUI 지원 (`--gui`) |
| `minisync-linux-amd64` | Linux | x86_64 | 정적 링크 (musl) |
| `minisync-linux-arm64` | Linux | aarch64 | 정적 링크 (musl) |

Linux 바이너리는 정적 링크되어 있어 **우분투 등 어떤 리눅스에서든 의존성 설치 없이 바로 실행**됩니다.
받은 뒤 실행 권한을 부여하세요: `chmod +x minisync-*`

> 직접 빌드하려면 아래 [소스에서 빌드](#소스에서-빌드)를 참고하세요 — 결과물은 `target/release/minisync`에 생성됩니다.

---

## 실행 방법

### 기본 문법

```
minisync [--gui] <동기화폴더> <내_주소> [상대방_주소 ...]
```

| 인자 | 설명 | 예시 |
|---|---|---|
| `<동기화폴더>` | 동기화할 폴더 경로 (없으면 자동 생성) | `~/Sync` |
| `<내_주소>` | 내가 수신할 IP:포트 | `0.0.0.0:9000` |
| `[상대방_주소]` | 연결할 상대 노드들 | `192.168.1.100:9001` |
| `--gui` | 데스크톱 GUI 실행 (macOS만) | |

### macOS (GUI 모드)

```bash
./minisync-macos --gui ~/Sync 0.0.0.0:9000
```

### 우분투 / Linux

```bash
# 1) 바이너리 복사
scp minisync-linux-amd64 ubuntu-server:~/minisync

# 2) 실행 권한 부여
ssh ubuntu-server "chmod +x ~/minisync"

# 3) 실행 (포트 9001에서 수신, macOS 노드에 연결)
ssh ubuntu-server "~/minisync ~/Sync 0.0.0.0:9001 <macOS-IP>:9000"
```

ARM64 서버 (라즈베리파이, AWS Graviton 등)는 `minisync-linux-arm64`를 사용하세요.

### 2대 연결 예시 (macOS + Ubuntu)

```bash
# macOS (192.168.1.50)
./minisync-macos --gui ~/Sync 0.0.0.0:9000 192.168.1.100:9001

# Ubuntu (192.168.1.100)
./minisync ~/Sync 0.0.0.0:9001 192.168.1.50:9000
```

양쪽 `~/Sync` 폴더에 파일을 넣으면 상대방에 자동으로 동기화됩니다.

### 3대 이상 (Full Mesh)

```bash
# Node A (10.0.0.1:9000)
./minisync ~/Sync 0.0.0.0:9000 10.0.0.2:9001 10.0.0.3:9002

# Node B (10.0.0.2:9001)
./minisync ~/Sync 0.0.0.0:9001 10.0.0.1:9000 10.0.0.3:9002

# Node C (10.0.0.3:9002)
./minisync ~/Sync 0.0.0.0:9002 10.0.0.1:9000 10.0.0.2:9001
```

### 저장된 설정으로 재실행

한번 실행하면 설정이 자동 저장됩니다. 다음부터는 인자 없이:

```bash
# macOS
./minisync-macos --gui

# Linux
./minisync
```

---

## 설정

### 전역 설정 (`~/.config/minisync/app.toml`)

자동 저장되며, 직접 편집도 가능:

```toml
sync_folder = "/home/user/Sync"
listen_addr = "0.0.0.0:9000"
peers = ["192.168.1.100:9001"]
node_name = "my-ubuntu-server"
```

- `node_name`: GUI에서 표시되는 노드 이름 (기본값: 호스트명). GUI Settings에서도 변경 가능.

### 폴더별 동기화 규칙 (`<동기화폴더>/.minisync/config.toml`)

파일 패턴별로 동기화 모드를 지정:

```toml
default_mode = "full_copy"

[[rules]]
pattern = "*.pdf"
mode = "reference"

[[rules]]
pattern = "*.mp4"
mode = "reference"

[[rules]]
pattern = "src/**"
mode = "full_copy"
```

- **full_copy**: 파일 전체가 모든 노드에 복사됨
- **reference**: 메타데이터만 공유, 필요할 때 수동 다운로드 (대용량 파일에 적합)

---

## 주요 기능

- **P2P Full Mesh**: 중앙 서버 없이 노드끼리 직접 연결
- **TLS 암호화**: 자체서명 인증서로 모든 통신 암호화
- **실시간 동기화**: 파일 변경 즉시 감지 및 전파
- **CRDT 텍스트 병합**: `.txt`, `.md`, `.json` 파일은 Automerge로 충돌 없이 병합
- **버전 벡터**: 바이너리 파일 충돌 감지 및 conflict 파일 생성
- **Reference 모드**: 대용량 파일은 메타데이터만 공유, 필요시 다운로드
- **노드 이름**: 각 컴퓨터를 사람이 읽을 수 있는 이름으로 식별
- **GUI** (macOS): 파일 목록, 피어 상태, 드래그&드롭 임포트, 설정 편집

---

## 소스에서 빌드

```bash
# Rust 설치 (https://rustup.rs)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# 헤드리스 빌드
cargo build --release

# GUI 포함 빌드 (macOS)
cargo build --release --features gui

# Linux 크로스 컴파일 (macOS에서)
# 사전 준비: cargo install cargo-zigbuild, zig 설치
rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl
cargo zigbuild --release --target x86_64-unknown-linux-musl
cargo zigbuild --release --target aarch64-unknown-linux-musl
```

---

## 네트워크 참고

- 수신 포트(예: 9000)가 방화벽에서 열려 있어야 합니다
- 같은 LAN이면 사설 IP로 바로 연결
- 인터넷 너머라면 포트포워딩 또는 Tailscale/WireGuard 등 VPN 사용

### LAN 자동 발견

같은 LAN의 노드들은 상대방 주소를 적지 않아도 UDP 비콘으로 서로를 찾아 자동 연결됩니다
(포트 19531). 상대방 주소 인자는 생략 가능:

```bash
minisync ~/Sync 0.0.0.0:9000      # 같은 LAN의 다른 노드를 자동 발견
```

### lattice VPN 오버레이 (`--lattice`)

[lattice](https://github.com/omnyx2) 메시 VPN 위에서 동기화할 수 있습니다. 각 노드의
lattice 데몬에 질의해 연결된 피어의 가상 IP로 자동 연결합니다 — NAT 너머에서도 동작:

```bash
minisync --lattice ~/Sync 0.0.0.0:9000
```

- 모든 노드가 **같은 수신 포트**를 써야 합니다(피어를 `<가상IP>:<그 포트>`로 연결).
- lattice 데몬이 실행 중이어야 하며, minisync 실행 파일 이름이 정확히 `minisync`여야
  데몬의 health-check 게이트를 통과합니다.
- `--lattice` 모드에서는 LAN UDP 자동 발견이 비활성화됩니다(오버레이가 유일 경로).
