//! 레인 라우팅: 파일 확장자로 CRDT 레인(텍스트) vs 파일 레인(바이너리)을 결정하고,
//! `.minisync/` 내부 경로를 식별하는 유틸리티.
//!
//! CRDT 레인 대상 확장자는 상수 배열로 관리한다.
//! 여기 없는 확장자(혹은 확장자 없는 파일)는 파일 레인으로 간다.

use std::path::{Path, PathBuf};

/// CRDT 레인 대상 확장자 (소문자, 점 제외).
/// 텍스트/마크업/데이터 + 주요 코드 확장자.
const CRDT_EXTENSIONS: &[&str] = &[
    // 텍스트·데이터
    "txt", "md", "json", "csv", "toml", "yaml", "yml", "xml", "svg", "ini",
    // 코드
    "rs", "py", "js", "ts", "jsx", "tsx", "go", "java", "c", "cpp", "h", "hpp",
    "cs", "rb", "swift", "kt", "kts", "lua", "pl", "php", "r", "sql",
    "html", "css", "scss", "less",
    "sh", "bash", "zsh", "fish",
    "makefile", "dockerfile",
    // 설정
    "cfg", "conf", "env", "gitignore", "editorconfig",
];

/// 동기화 내부 상태 디렉터리 이름.
pub const MINISYNC_DIR: &str = ".minisync";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    /// 텍스트 파일 — Automerge CRDT로 동시 편집 병합.
    Crdt,
    /// 바이너리/기타 — 통째 전송 + LWW.
    File,
}

/// 상대 경로의 확장자를 보고 레인을 결정한다.
pub fn lane_for(rel_path: &str) -> Lane {
    if let Some(ext) = Path::new(rel_path).extension() {
        let ext_lower = ext.to_string_lossy().to_lowercase();
        if CRDT_EXTENSIONS.contains(&ext_lower.as_str()) {
            return Lane::Crdt;
        }
    }
    Lane::File
}

/// 상대 경로가 `.minisync/` 내부(동기화 대상 아님)인지 확인.
pub fn is_minisync_internal(rel_path: &str) -> bool {
    let normalized = rel_path.replace('\\', "/");
    normalized == MINISYNC_DIR
        || normalized.starts_with(&format!("{MINISYNC_DIR}/"))
}

/// `.minisync/crdt/<rel>.amrg` 경로 (Automerge 문서 저장 위치).
pub fn crdt_state_path(root: &Path, rel: &str) -> PathBuf {
    root.join(MINISYNC_DIR).join("crdt").join(format!("{rel}.amrg"))
}

/// `.minisync/shadow/<rel>` 경로 (마지막 반영 내용, diff 기준).
pub fn shadow_path(root: &Path, rel: &str) -> PathBuf {
    root.join(MINISYNC_DIR).join("shadow").join(rel)
}

/// `.minisync/versions/<rel>.vv` 경로 (파일 레인 버전벡터).
pub fn version_path(root: &Path, rel: &str) -> PathBuf {
    root.join(MINISYNC_DIR).join("versions").join(format!("{rel}.vv"))
}

// ────────────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crdt_lane_extensions() {
        assert_eq!(lane_for("notes.txt"), Lane::Crdt);
        assert_eq!(lane_for("README.md"), Lane::Crdt);
        assert_eq!(lane_for("config.json"), Lane::Crdt);
        assert_eq!(lane_for("data.csv"), Lane::Crdt);
        assert_eq!(lane_for("src/main.rs"), Lane::Crdt);
        assert_eq!(lane_for("app.py"), Lane::Crdt);
        assert_eq!(lane_for("index.html"), Lane::Crdt);
    }

    #[test]
    fn file_lane_extensions() {
        assert_eq!(lane_for("report.pdf"), Lane::File);
        assert_eq!(lane_for("image.png"), Lane::File);
        assert_eq!(lane_for("archive.zip"), Lane::File);
        assert_eq!(lane_for("video.mp4"), Lane::File);
        // 확장자 없는 파일도 파일 레인
        assert_eq!(lane_for("Makefile"), Lane::File);
    }

    #[test]
    fn minisync_internal_detection() {
        assert!(is_minisync_internal(".minisync"));
        assert!(is_minisync_internal(".minisync/crdt/notes.txt.amrg"));
        assert!(is_minisync_internal(".minisync/shadow/notes.txt"));
        // 아닌 것
        assert!(!is_minisync_internal("notes.txt"));
        assert!(!is_minisync_internal("dir/.minisync_not"));
        assert!(!is_minisync_internal(".minisyncx/foo"));
    }

    #[test]
    fn path_helpers() {
        let root = Path::new("/tmp/sync");
        assert_eq!(
            crdt_state_path(root, "notes.txt"),
            PathBuf::from("/tmp/sync/.minisync/crdt/notes.txt.amrg")
        );
        assert_eq!(
            shadow_path(root, "notes.txt"),
            PathBuf::from("/tmp/sync/.minisync/shadow/notes.txt")
        );
    }
}
