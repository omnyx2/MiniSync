//! CRDT↔디스크 브리지: 파일 편집(텍스트)을 Automerge 연산으로 변환하고,
//! Automerge 문서에서 텍스트를 재구성한다.
//!
//! 핵심 흐름:
//!   로컬 편집: 새 내용 읽기 → shadow와 diff → splice_text → Automerge 저장
//!   원격 변경: change 적용 → doc.text() 재구성 → 파일 쓰기 + shadow 갱신
//!
//! diff에는 `similar` crate(문자 단위), CRDT에는 `automerge` crate를 사용한다.

use automerge::{AutoCommit, ObjType, ReadDoc, transaction::Transactable, ROOT};
use similar::DiffOp;
use std::fs;
use std::path::Path;

use crate::routing;

/// Automerge 문서 안에서 텍스트 객체를 가리키는 키 이름.
const TEXT_KEY: &str = "text";

/// 새 Automerge 문서를 만들고 `initial` 내용으로 초기화한다.
/// 반환: (doc, text_object_id)
pub fn new_doc(initial: &str) -> AutoCommit {
    let mut doc = AutoCommit::new();
    let text_id = doc
        .put_object(ROOT, TEXT_KEY, ObjType::Text)
        .expect("put_object on new doc");
    if !initial.is_empty() {
        doc.splice_text(&text_id, 0, 0, initial)
            .expect("initial splice_text");
    }
    doc
}

/// 문서에서 텍스트 객체 ID를 가져온다.
pub fn text_id(doc: &AutoCommit) -> automerge::ObjId {
    match doc.get(ROOT, TEXT_KEY).unwrap() {
        Some((automerge::Value::Object(ObjType::Text), id)) => id,
        _ => panic!("no text object at ROOT/{TEXT_KEY}"),
    }
}

/// 문서의 현재 텍스트 내용을 반환한다.
pub fn doc_text(doc: &AutoCommit) -> String {
    let id = text_id(doc);
    doc.text(&id).expect("text()")
}

/// 로컬 파일 편집을 Automerge 문서에 반영한다.
///
/// `shadow`: 마지막으로 알려진 내용 (= Automerge 문서가 현재 갖고 있는 텍스트와 동일해야 함).
/// `new_content`: 디스크에서 새로 읽은 파일 내용.
///
/// shadow와 new_content를 문자 단위로 diff한 뒤, 각 차이를 `splice_text`로 적용한다.
pub fn apply_local_edit(doc: &mut AutoCommit, shadow: &str, new_content: &str) {
    let id = text_id(doc);
    let splices = diff_to_splices(shadow, new_content);
    // 앞에서부터 순서대로 적용하되, 이전 splice로 인한 길이 변화를 offset으로 보정.
    let mut offset: isize = 0;
    for (pos, del, ref ins) in splices {
        let adjusted = (pos as isize + offset) as usize;
        doc.splice_text(&id, adjusted, del as isize, ins)
            .expect("splice_text");
        offset += ins.chars().count() as isize - del as isize;
    }
}

/// shadow(이전 내용)와 new(현재 내용)를 문자 단위로 diff해서
/// `(old_char_pos, delete_count, insert_text)` 목록을 반환한다.
fn diff_to_splices(old: &str, new: &str) -> Vec<(usize, usize, String)> {
    let diff = similar::TextDiff::configure()
        .algorithm(similar::Algorithm::Patience)
        .diff_chars(old, new);

    let new_chars: Vec<char> = new.chars().collect();
    let mut splices = Vec::new();

    for op in diff.ops() {
        match *op {
            DiffOp::Equal { .. } => {}
            DiffOp::Delete { old_index, old_len, .. } => {
                splices.push((old_index, old_len, String::new()));
            }
            DiffOp::Insert { old_index, new_index, new_len, .. } => {
                let text: String = new_chars[new_index..new_index + new_len].iter().collect();
                splices.push((old_index, 0, text));
            }
            DiffOp::Replace { old_index, old_len, new_index, new_len, .. } => {
                let text: String = new_chars[new_index..new_index + new_len].iter().collect();
                splices.push((old_index, old_len, text));
            }
        }
    }
    splices
}

// ────────────────────────────────────────────────────────────────────────────
// 디스크 저장·로드 (`.minisync/crdt/`, `.minisync/shadow/`)
// ────────────────────────────────────────────────────────────────────────────

/// `.minisync/crdt/` 또는 파일 내용으로부터 Automerge 문서를 로드(또는 새로 생성).
/// 생성 시 `save()`를 한 번 호출해 save-point를 잡아 둔다 — 이후
/// `save_incremental()`이 새 변경분만 반환하도록.
pub fn load_or_create_doc(root: &Path, rel: &str) -> AutoCommit {
    let amrg = routing::crdt_state_path(root, rel);
    if amrg.exists() {
        if let Ok(data) = fs::read(&amrg) {
            if let Ok(doc) = AutoCommit::load(&data) {
                return doc;
            }
        }
    }
    // 기존 파일 내용으로 새 문서 생성
    let content = fs::read_to_string(root.join(rel)).unwrap_or_default();
    let mut doc = new_doc(&content);
    save_doc_to_disk(root, rel, &mut doc); // save-point 확립 + 디스크 기록
    // shadow도 초기화
    write_shadow(root, rel, &content);
    doc
}

/// Automerge 문서를 `.minisync/crdt/<rel>.amrg`에 저장.
/// `doc.save()`를 호출하므로 save-point도 갱신된다.
pub fn save_doc_to_disk(root: &Path, rel: &str, doc: &mut AutoCommit) {
    let amrg = routing::crdt_state_path(root, rel);
    if let Some(p) = amrg.parent() {
        let _ = fs::create_dir_all(p);
    }
    let _ = fs::write(&amrg, doc.save());
}

/// shadow 파일 읽기. 없으면 빈 문자열.
pub fn read_shadow(root: &Path, rel: &str) -> String {
    fs::read_to_string(routing::shadow_path(root, rel)).unwrap_or_default()
}

/// shadow 파일 쓰기.
pub fn write_shadow(root: &Path, rel: &str, content: &str) {
    let p = routing::shadow_path(root, rel);
    if let Some(parent) = p.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(&p, content);
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// 기본: shadow diff → splice → 재구성이 정확한지.
    #[test]
    fn single_edit_roundtrip() {
        let initial = "hello world";
        let mut doc = new_doc(initial);
        assert_eq!(doc_text(&doc), initial);

        // 편집: "hello world" → "hello beautiful world"
        apply_local_edit(&mut doc, initial, "hello beautiful world");
        assert_eq!(doc_text(&doc), "hello beautiful world");

        // 또 편집: 끝에 느낌표
        apply_local_edit(&mut doc, "hello beautiful world", "hello beautiful world!");
        assert_eq!(doc_text(&doc), "hello beautiful world!");
    }

    /// 삭제 편집.
    #[test]
    fn delete_edit() {
        let initial = "abcdef";
        let mut doc = new_doc(initial);

        apply_local_edit(&mut doc, initial, "adf");
        assert_eq!(doc_text(&doc), "adf");
    }

    /// 교체(replace) 편집.
    #[test]
    fn replace_edit() {
        let initial = "hello world";
        let mut doc = new_doc(initial);

        apply_local_edit(&mut doc, initial, "hi earth");
        assert_eq!(doc_text(&doc), "hi earth");
    }

    /// 핵심 테스트: 두 피어가 같은 초기 텍스트에서 갈라져 동시 편집한 뒤
    /// merge하면 양쪽이 동일한 텍스트로 수렴한다.
    #[test]
    fn concurrent_edits_converge() {
        let initial = "hello world";

        // 두 피어: doc1과 doc2를 같은 초기 상태에서 fork.
        let mut doc1 = new_doc(initial);
        let mut doc2 = doc1.fork();

        // 피어 1: "hello world" → "hello beautiful world"
        apply_local_edit(&mut doc1, initial, "hello beautiful world");
        assert_eq!(doc_text(&doc1), "hello beautiful world");

        // 피어 2 (동시): "hello world" → "hello world!"
        apply_local_edit(&mut doc2, initial, "hello world!");
        assert_eq!(doc_text(&doc2), "hello world!");

        // 변경사항 교환 (merge)
        doc1.merge(&mut doc2).expect("merge doc2 into doc1");
        doc2.merge(&mut doc1).expect("merge doc1 into doc2");

        let text1 = doc_text(&doc1);
        let text2 = doc_text(&doc2);

        println!("doc1: {text1:?}");
        println!("doc2: {text2:?}");

        // 양쪽이 같은 텍스트로 수렴해야 한다.
        assert_eq!(text1, text2, "concurrent edits must converge to same text");
        // 두 편집이 모두 반영되어야 한다.
        assert!(text1.contains("beautiful"), "should contain peer 1's insertion");
        assert!(text1.ends_with('!'), "should contain peer 2's insertion");
    }

    /// 더 복잡한 동시 편집: 같은 위치 근처를 양쪽이 수정.
    #[test]
    fn concurrent_edits_overlapping() {
        let initial = "the quick brown fox";

        let mut doc1 = new_doc(initial);
        let mut doc2 = doc1.fork();

        // 피어 1: "the quick brown fox" → "the slow brown fox"
        apply_local_edit(&mut doc1, initial, "the slow brown fox");
        // 피어 2: "the quick brown fox" → "the quick red fox"
        apply_local_edit(&mut doc2, initial, "the quick red fox");

        doc1.merge(&mut doc2).expect("merge");
        doc2.merge(&mut doc1).expect("merge");

        let text1 = doc_text(&doc1);
        let text2 = doc_text(&doc2);

        println!("doc1: {text1:?}");
        println!("doc2: {text2:?}");

        // 수렴: 양쪽 동일.
        assert_eq!(text1, text2, "must converge");
    }

    /// incremental save/load를 통한 변경 교환 (네트워크 시뮬레이션).
    #[test]
    fn incremental_change_exchange() {
        let initial = "line one\nline two\n";

        let mut doc1 = new_doc(initial);
        // doc2는 doc1을 직렬화해서 로드 (네트워크로 받은 것처럼).
        let saved = doc1.save();
        let mut doc2 = AutoCommit::load(&saved).expect("load");

        // 피어 1 편집
        let new1 = "line one\nline ONE EDITED\n";
        apply_local_edit(&mut doc1, initial, new1);
        let changes1 = doc1.save_incremental();

        // 피어 2 편집 (동시)
        let new2 = "line one\nline two\nline three\n";
        apply_local_edit(&mut doc2, initial, new2);
        let changes2 = doc2.save_incremental();

        // 교환
        doc1.load_incremental(&changes2).expect("load changes2");
        doc2.load_incremental(&changes1).expect("load changes1");

        let text1 = doc_text(&doc1);
        let text2 = doc_text(&doc2);

        println!("doc1: {text1:?}");
        println!("doc2: {text2:?}");

        assert_eq!(text1, text2, "incremental exchange must converge");
        assert!(text1.contains("ONE EDITED"), "peer 1 edit present");
        assert!(text1.contains("line three"), "peer 2 edit present");
    }
}
