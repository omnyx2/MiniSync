//! CRDT↔디스크 브리지: 파일 편집(텍스트)을 Automerge 연산으로 변환하고,
//! Automerge 문서에서 텍스트를 재구성한다.
//!
//! 핵심 흐름:
//!   로컬 편집: 새 내용 읽기 → shadow와 diff → splice_text → Automerge 저장
//!   원격 변경: change 적용 → doc.text() 재구성 → 파일 쓰기 + shadow 갱신
//!
//! diff에는 `similar` crate(문자 단위), CRDT에는 `automerge` crate를 사용한다.

use automerge::transaction::CommitOptions;
use automerge::{ActorId, AutoCommit, ObjType, ReadDoc, transaction::Transactable, ROOT};
use similar::DiffOp;
use std::fs;
use std::path::Path;

use crate::routing;

/// Automerge 문서 안에서 텍스트 객체를 가리키는 키 이름.
const TEXT_KEY: &str = "text";

/// genesis(빈 text 객체 생성) 연산에만 쓰는 고정 actor.
/// 모든 피어/모든 파일에서 동일하므로, 두 노드가 같은 파일을 **독립적으로**
/// 생성해도 genesis change가 바이트 단위로 동일(같은 ChangeHash)해진다.
/// → `text` 객체의 ObjId가 같아져 두 문서가 깔끔히 merge된다.
const GENESIS_ACTOR: [u8; 16] = [0u8; 16];

/// genesis 골격: 고정 actor + 고정 timestamp(0)로 빈 text 객체만 만든 문서.
/// 어떤 피어에서 호출해도 직렬화 결과가 동일하다(merge 시 dedup됨).
fn new_doc_skeleton() -> AutoCommit {
    let mut doc = AutoCommit::new().with_actor(ActorId::from(GENESIS_ACTOR));
    doc.put_object(ROOT, TEXT_KEY, ObjType::Text)
        .expect("genesis put_object");
    // 고정 시각으로 커밋 → 피어 간 genesis change 해시가 일치.
    doc.commit_with(CommitOptions::default().with_time(0));
    doc
}

/// 새 Automerge 문서를 만들고 `initial` 내용으로 초기화한다.
///
/// **초기 내용까지** genesis(고정 actor + 고정 time)에 담는다. 두 피어가 같은
/// 내용으로 파일을 독립 생성하면 genesis change가 바이트 동일 → merge 시 dedup
/// → 내용이 중복되지 않는다. 이후의 실제 편집만 피어별 고유 actor로 기록해
/// 동시편집이 충돌 없이 병합되게 한다.
pub fn new_doc(initial: &str) -> AutoCommit {
    let mut doc = AutoCommit::new().with_actor(ActorId::from(GENESIS_ACTOR));
    let text_id = doc
        .put_object(ROOT, TEXT_KEY, ObjType::Text)
        .expect("genesis put_object");
    if !initial.is_empty() {
        doc.splice_text(&text_id, 0, 0, initial)
            .expect("initial splice_text");
    }
    // 초기 내용을 포함한 genesis를 고정 시각으로 봉인.
    doc.commit_with(CommitOptions::default().with_time(0));
    // 이후 편집은 고유 actor로.
    doc.set_actor(ActorId::random());
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

/// CRDT 문서를 **미리** 만들어 둔다(아직 없을 때만). 파일 내용이 양쪽에서
/// 동일한 시점(편집 전)에 호출되면, 결정적 genesis 덕분에 두 피어가 바이트
/// 동일한 문서를 독립 생성 → 이후 동시 편집이 같은 text 객체 위에서 병합된다.
/// 편집이 먼저 일어나 내용이 갈린 뒤 각자 만들면 genesis가 어긋나 데이터가
/// 유실되므로, 스캐너가 내용이 같을 때 선제적으로 호출한다. .amrg가 이미 있으면
/// 즉시 반환(경로 확인만).
pub fn ensure_doc(root: &Path, rel: &str) {
    if routing::crdt_state_path(root, rel).exists() {
        return;
    }
    // UTF-8 텍스트만(읽기 실패 시 건너뜀).
    let content = match fs::read_to_string(root.join(rel)) {
        Ok(c) => c,
        Err(_) => return,
    };
    let mut doc = new_doc(&content);
    save_doc_to_disk(root, rel, &mut doc);
    write_shadow(root, rel, &content);
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

    /// genesis 골격은 모든 호출에서 바이트 동일해야 한다(피어 간 dedup 전제).
    #[test]
    fn genesis_skeleton_is_deterministic() {
        let mut a = new_doc_skeleton();
        let mut b = new_doc_skeleton();
        assert_eq!(a.save(), b.save(), "genesis skeleton must be byte-identical");
        // text 객체 ObjId도 같아야 한다.
        assert_eq!(text_id(&a), text_id(&b));
    }

    /// 실전 버그 재현: 같은 내용의 파일이 양쪽에 이미 있고(=동일 base) CRDT
    /// 상태가 비어 두 피어가 문서를 **독립 생성**한 뒤, 서로 다른 줄을 편집한다.
    /// content-in-genesis 덕분에 base는 dedup되어 **중복 없이** 수렴해야 한다.
    /// (이전엔 base가 양쪽 고유 actor로 중복 삽입되어 문서가 2배가 됐다.)
    #[test]
    fn independently_created_same_base_no_duplication() {
        let base = "line1\nline2\nline3\nline4\n";
        let mut doc1 = new_doc(base);
        let mut doc2 = new_doc(base);

        // 같은 base로 만들었으면 genesis가 바이트 동일해야 한다.
        assert_eq!(
            new_doc_skeleton().save(),
            new_doc_skeleton().save(),
            "empty skeleton deterministic"
        );

        // 피어1은 line2를, 피어2는 line4를 편집(동시).
        apply_local_edit(&mut doc1, base, "line1\nLINE2-BY-1\nline3\nline4\n");
        apply_local_edit(&mut doc2, base, "line1\nline2\nline3\nLINE4-BY-2\n");

        // CrdtSync 교환 + merge.
        let mut recv2 = AutoCommit::load(&doc2.save()).unwrap();
        let mut recv1 = AutoCommit::load(&doc1.save()).unwrap();
        doc1.merge(&mut recv2).unwrap();
        doc2.merge(&mut recv1).unwrap();

        let t1 = doc_text(&doc1);
        let t2 = doc_text(&doc2);
        println!("doc1={t1:?}\ndoc2={t2:?}");
        assert_eq!(t1, t2, "must converge");
        assert!(t1.contains("LINE2-BY-1"), "peer1 edit present");
        assert!(t1.contains("LINE4-BY-2"), "peer2 edit present");
        // 중복 금지: 4줄짜리 base가 두 번 들어가면 안 된다.
        assert_eq!(
            t1.lines().count(),
            4,
            "no duplication: must stay 4 lines, got {:?}",
            t1
        );
        assert_eq!(t1.matches("line1").count(), 1, "base not duplicated");
    }

    /// handle_crdt_sync 프로토콜 모사: CrdtSync(전체 doc) 수신 → merge →
    /// save_after(받은 heads)로 회신 → 송신측 load_incremental → 수렴.
    #[test]
    fn sync_then_reply_roundtrip_converges() {
        // 같은 base에서 독립 생성 후 서로 다른 줄 편집.
        let base = "AAA\nBBB\n";
        let mut p1 = new_doc(base);
        let mut p2 = new_doc(base);
        apply_local_edit(&mut p1, base, "AAA-1\nBBB\n");
        apply_local_edit(&mut p2, base, "AAA\nBBB-2\n");

        // 1) p1 → p2 로 CrdtSync(전체 doc) 전송.
        let sync_from_p1 = p1.save();
        // 2) p2 수신 측: merge 후, p1이 모르는 변경을 회신.
        let mut received = AutoCommit::load(&sync_from_p1).unwrap();
        let received_heads = received.get_heads();
        p2.merge(&mut received).unwrap();
        let reply = p2.save_after(&received_heads);
        // 3) p1 수신: 회신을 load_incremental.
        p1.load_incremental(&reply).unwrap();

        let t1 = doc_text(&p1);
        let t2 = doc_text(&p2);
        println!("p1={t1:?} p2={t2:?}");
        assert_eq!(t1, t2, "sync+reply must converge");
        assert!(t1.contains("AAA-1") && t1.contains("BBB-2"), "both edits survive");
        assert_eq!(t1.lines().count(), 2, "no duplication, got {t1:?}");
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
