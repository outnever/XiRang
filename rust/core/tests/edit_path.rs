//! 单节点直读编辑（`edit::edit_node`）的护栏。
//!
//! 这条路的卖点是**不整份载入**（347 万节点的文件上，整份载入约 3 秒，
//! 直读是毫秒级）；护栏要保证它写出来的东西与老路（整份载入 + `Store::update`）
//! 语义一致，而且对不上时**明确拒绝**、绝不猜。

use std::path::{Path, PathBuf};

use xirang_core::codec::{Node, Uuid, Value};
use xirang_core::tree::Store;
use xirang_core::{edit, tree, wsidx};

fn tmp_dir(tag: &str) -> PathBuf {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let d = std::env::temp_dir().join(format!("xr-edit-{tag}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// 建一个三节点的小文件：根 →（词形 / @history 由改动自己产生）。
fn make_file(dir: &Path, name: &str) -> (PathBuf, Uuid) {
    let path = dir.join(name);
    let mut s = Store::new();
    let root = s.create(None, "根", Value::Empty, false).id;
    s.create(Some(root), "词形", Value::Text("灯".into()), false);
    s.save(&path).unwrap();
    (path, root)
}

/// 把整棵树渲染成「名字=值」的序列；`@replaced` 的时间戳归一化，
/// 好让「直读写出来的」与「整份写出来的」能逐条对比。
fn render(path: &Path) -> Vec<String> {
    let s = tree::Store::load_view(path).unwrap();
    let mut out = Vec::new();
    fn walk(s: &Store, n: &Node, out: &mut Vec<String>) {
        let v = match &n.value {
            Value::Empty => String::new(),
            Value::Text(t) if n.name == "@replaced" => "<时间>".into(),
            Value::Text(t) => t.clone(),
            Value::Reference(r) => format!("→{r}"),
            other => format!("{other:?}"),
        };
        out.push(format!("{}={}", n.name, v));
        for c in s.children(n) {
            walk(s, c, out);
        }
    }
    for r in s.roots() {
        walk(&s, r, &mut out);
    }
    out
}

fn child_id(path: &Path, name: &str) -> Uuid {
    let s = tree::Store::load_view(path).unwrap();
    s.nodes().iter().find(|n| n.name == name).expect("找得到").id
}

/// 直读编辑与「整份载入 + `Store::update`」应当写出同一棵可见的树。
#[test]
fn direct_edit_matches_full_load_edit() {
    let dir = tmp_dir("same");
    let (a, _ra) = make_file(&dir, "直读.xirang");
    let (b, _rb) = make_file(&dir, "整份.xirang");
    let a_word = child_id(&a, "词形");
    let b_word = child_id(&b, "词形");

    wsidx::rebuild(&dir, &[]).unwrap();

    let out = edit::edit_node(&a, a_word, None, Some(Value::Text("火".into())), true).unwrap();
    assert_eq!(out.before.value, Value::Text("灯".into()));
    assert_eq!(out.after.value, Value::Text("火".into()));
    assert!(
        out.appended >= 4,
        "至少：快照 + @replaced + 新记录 + 协议声明，实得 {}",
        out.appended
    );

    // 老路：整份载入 → 改 → 幂等补协议声明（CLI 的 ops::save 会做这一步）→ 落盘
    let mut s = Store::load_view(&b).unwrap();
    s.update(b_word, Value::Text("火".into())).unwrap();
    let b_root = s.root_of(b_word).unwrap();
    if !s.declares_protocol(b_root, tree::PROTOCOL_APPEND) {
        s.create(
            Some(b_root),
            "@protocol",
            Value::Text(tree::PROTOCOL_APPEND.into()),
            false,
        );
    }
    s.save(&b).unwrap();

    assert_eq!(render(&a), render(&b), "直读写出来的树与整份写的应当一致");
    let got = tree::Store::load_view(&a).unwrap().get(a_word).cloned().unwrap();
    assert_eq!(got.value, Value::Text("火".into()));

    std::fs::remove_dir_all(&dir).ok();
    std::fs::remove_dir_all(wsidx::index_dir(&dir)).ok();
}

/// 没有台账（或台账对不上）必须拒绝——调用方据此退回整份载入。
#[test]
fn direct_edit_refuses_without_a_ledger() {
    let dir = tmp_dir("noledger");
    let (path, _root) = make_file(&dir, "无台账.xirang");
    let word = child_id(&path, "词形");
    assert!(
        edit::edit_node(&path, word, None, Some(Value::Text("火".into())), true).is_err(),
        "没建台账就该拒绝，而不是猜"
    );
    wsidx::rebuild(&dir, &[]).unwrap();
    assert!(edit::edit_node(&path, word, None, Some(Value::Text("火".into())), true).is_ok());
    std::fs::remove_dir_all(&dir).ok();
    std::fs::remove_dir_all(wsidx::index_dir(&dir)).ok();
}

/// 连改多次：`@protocol = append-v1` 只加一次，台账一直跟得上，文件只长一点点。
#[test]
fn direct_edit_keeps_marker_and_index_in_sync() {
    let dir = tmp_dir("marker");
    let (path, _root) = make_file(&dir, "连续改.xirang");
    let word = child_id(&path, "词形");
    wsidx::rebuild(&dir, &[]).unwrap();

    let size_before = std::fs::metadata(&path).unwrap().len();
    for i in 0..5 {
        edit::edit_node(&path, word, None, Some(Value::Text(format!("第{i}次"))), true).unwrap();
    }
    let size_after = std::fs::metadata(&path).unwrap().len();
    assert!(size_after > size_before);
    assert!(
        size_after - size_before < 3_000,
        "五次改动不该长这么多：{size_before} → {size_after}"
    );

    let text = render(&path).join("\n");
    assert_eq!(text.matches("@protocol=append-v1").count(), 1, "协议声明必须幂等：\n{text}");
    assert_eq!(
        tree::Store::load_view(&path).unwrap().get(word).unwrap().value,
        Value::Text("第4次".into())
    );

    let st = wsidx::files_status(&dir).unwrap();
    assert!(st.iter().all(|f| f.fresh), "改完台账要跟得上：{st:?}");
    wsidx::Reader::open(&dir).unwrap();
    std::fs::remove_dir_all(&dir).ok();
    std::fs::remove_dir_all(wsidx::index_dir(&dir)).ok();
}

/// 模板定义里的节点受保护（与 `Store::is_editable` 同一套判断）。
#[test]
fn direct_edit_recognises_template_definitions() {
    let dir = tmp_dir("tpl");
    let path = dir.join("模板.xirang");
    let mut s = Store::new();
    let tpl = s.create(None, "词条", Value::Empty, false).id;
    s.create(Some(tpl), "@模板", Value::Empty, false);
    let field = s.create(Some(tpl), "词形", Value::Empty, false).id;
    let inst = s.create(None, "实例", Value::Empty, false).id;
    s.create(Some(inst), "@实例", Value::Empty, false);
    s.create(Some(inst), "@模板", Value::Reference(tpl), false);
    let inst_field = s.create(Some(inst), "词形", Value::Text("灯".into()), false).id;
    s.save(&path).unwrap();
    wsidx::rebuild(&dir, &[]).unwrap();

    assert!(edit::under_template(&path, field).unwrap(), "模板定义里的字段受保护");
    assert!(edit::under_template(&path, tpl).unwrap(), "模板定义根自己也算");
    assert!(!edit::under_template(&path, inst_field).unwrap(), "实例可以编辑");
    std::fs::remove_dir_all(&dir).ok();
    std::fs::remove_dir_all(wsidx::index_dir(&dir)).ok();
}
