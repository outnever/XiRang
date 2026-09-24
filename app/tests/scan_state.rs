//! 流式搜索 / 校验、视图态持久化的测试（都不需要界面）。

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use xirang_app::scan::{self, Query};
use xirang_app::state::{FileView, ViewState};
use xirang_core::codec::{Uuid, Value};
use xirang_core::index::{sidecar_path, AppendWriter};
use xirang_core::tree::{self, Store};

fn tmp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("xr_scan_{name}_{}.xirang", Uuid::random_v4()))
}

fn cleanup(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(sidecar_path(path));
}

fn sample(path: &std::path::Path) -> (Uuid, Uuid) {
    let mut s = Store::new();
    let root = s.create(None, "灯", Value::Empty, false).id;
    let form = s.create(Some(root), "词形", Value::Text("灯".into()), false).id;
    s.create(Some(root), "释义", Value::Text("照明器具".into()), false);
    s.create(Some(root), "数量", Value::Int(3), false);
    s.save(path).unwrap();
    (root, form)
}

#[test]
fn search_by_name_value_and_kind() {
    let p = tmp("search");
    sample(&p);
    let cancel = AtomicBool::new(false);

    let by_name = scan::search(&p, &Query::Name("词".into()), &cancel).unwrap();
    assert_eq!(by_name.len(), 1);
    assert_eq!(by_name[0].name, "词形");

    let by_value = scan::search(&p, &Query::Value("照明".into()), &cancel).unwrap();
    assert_eq!(by_value.len(), 1);
    assert_eq!(by_value[0].name, "释义");

    let by_kind = scan::search(&p, &Query::Kind("整数".into()), &cancel).unwrap();
    assert_eq!(by_kind.len(), 1, "只有「数量」是整数");
    assert_eq!(by_kind[0].name, "数量");
    cleanup(&p);
}

#[test]
fn search_follows_append_v1_last_write_wins() {
    let p = tmp("search_append");
    let (root, form) = sample(&p);
    let (mut w, _) = AppendWriter::open(&p).unwrap();
    // 把「词形」改名成「字形」
    w.append_node(&xirang_core::codec::Node {
        id: form,
        parent: Some(root),
        name: "字形".into(),
        value: Value::Text("灯".into()),
    })
    .unwrap();
    w.sync().unwrap();

    let cancel = AtomicBool::new(false);
    let old = scan::search(&p, &Query::Name("词形".into()), &cancel).unwrap();
    assert!(old.is_empty(), "旧名字不该再命中（后写覆盖）");
    let new = scan::search(&p, &Query::Name("字形".into()), &cancel).unwrap();
    assert_eq!(new.len(), 1);
    cleanup(&p);
}

#[test]
fn streaming_validate_matches_view_semantics() {
    let p = tmp("validate");
    let (root, _) = sample(&p);
    let cancel = AtomicBool::new(false);

    let mut progress = |_: u64| {};
    let issues = scan::validate_stream(&p, &cancel, &mut progress).unwrap();
    assert!(issues.is_empty(), "干净文件应当没有错误");

    // 追加一条同编号修订：没有声明协议 → E002
    let (mut w, _) = AppendWriter::open(&p).unwrap();
    w.append_node(&xirang_core::codec::Node {
        id: root,
        parent: None,
        name: "灯".into(),
        value: Value::Text("改".into()),
    })
    .unwrap();
    w.sync().unwrap();
    let issues = scan::validate_stream(&p, &cancel, &mut progress).unwrap();
    assert!(issues.iter().any(|i| i.code == "E002"), "未声明修订 → 编号冲突");

    // 补上 @protocol = append-v1 之后不再报
    let (mut w, _) = AppendWriter::open(&p).unwrap();
    w.append_node(&xirang_core::codec::Node {
        id: Uuid::random_v4(),
        parent: Some(root),
        name: "@protocol".into(),
        value: Value::Text(tree::PROTOCOL_APPEND.into()),
    })
    .unwrap();
    w.sync().unwrap();
    let issues = scan::validate_stream(&p, &cancel, &mut progress).unwrap();
    assert!(
        !issues.iter().any(|i| i.code == "E002"),
        "声明了修订协议 → 重复编号不是冲突"
    );
    cleanup(&p);
}

#[test]
fn streaming_validate_flags_dangling_reference() {
    let p = tmp("dangling");
    let mut s = Store::new();
    let root = s.create(None, "根", Value::Empty, false).id;
    let ghost = Uuid::random_v4();
    s.create(Some(root), "指向不存在", Value::Reference(ghost), false);
    s.save(&p).unwrap();

    let cancel = AtomicBool::new(false);
    let mut progress = |_: u64| {};
    let issues = scan::validate_stream(&p, &cancel, &mut progress).unwrap();
    assert!(issues.iter().any(|i| i.code == "R001"), "引用断裂应被发现");
    cleanup(&p);
}

#[test]
fn view_state_roundtrip_and_recent() {
    let a = Uuid::random_v4();
    let b = Uuid::random_v4();
    let mut st = ViewState::default();
    st.touch_recent(std::path::Path::new("/tmp/甲.xirang"));
    st.touch_recent(std::path::Path::new("/tmp/乙.xirang"));
    st.set_view(
        std::path::Path::new("/tmp/甲.xirang"),
        FileView {
            layout: "layered".into(),
            expanded: vec![a, b],
            focus: Some(a),
        },
    );

    let text = st.to_text();
    let back = ViewState::from_text(&text);
    assert_eq!(back.recent, vec!["/tmp/乙.xirang", "/tmp/甲.xirang"]);
    let view = back
        .view_of(std::path::Path::new("/tmp/甲.xirang"))
        .expect("视图态应当能读回来");
    assert_eq!(view.layout, "layered");
    assert_eq!(view.expanded, vec![a, b]);
    assert_eq!(view.focus, Some(a));

    // 最近文件去重：再点一次「甲」应当排到最前
    st.touch_recent(std::path::Path::new("/tmp/甲.xirang"));
    assert_eq!(st.recent.first().unwrap(), "/tmp/甲.xirang");
    assert_eq!(st.recent.len(), 2, "不重复累积");

    // 语言与配色也一起持久化
    let mut st2 = ViewState::default();
    st2.lang = "en".into();
    st2.palette.bg = "#101010".into();
    st2.palette.dark = false;
    let back2 = ViewState::from_text(&st2.to_text());
    assert_eq!(back2.lang, "en");
    assert_eq!(back2.palette.bg, "#101010");
    assert!(!back2.palette.dark);
}

/// 大文件实测（手动跑）：
/// 打印某个文件的引用边数（核对「全局图」的取数）：
/// `XIRANG_FILE=... cargo test -p xirang-app --test scan_state -- --ignored --nocapture`
#[test]
#[ignore]
fn report_edges_of_file() {
    let Ok(path) = std::env::var("XIRANG_FILE") else {
        return;
    };
    let mut doc = xirang_app::lazy::Doc::open(std::path::Path::new(&path)).unwrap();
    let edges = doc.edges();
    println!("{path}: {} 条引用边", edges.len());
}

/// 大文件实测（手动跑）：
/// `XIRANG_BIG=/path/big.xirang cargo test -p xirang-app --test scan_state -- --ignored --nocapture`
#[test]
#[ignore]
fn bench_scan_on_big_file() {
    let Ok(path) = std::env::var("XIRANG_BIG") else {
        return;
    };
    let path = std::path::PathBuf::from(path);
    let cancel = AtomicBool::new(false);

    let t = std::time::Instant::now();
    let hits = scan::search(&path, &Query::Name("动物界".into()), &cancel).unwrap();
    println!("流式搜索「动物界」：{} 命中，用时 {} ms", hits.len(), t.elapsed().as_millis());

    let t = std::time::Instant::now();
    let mut progress = |_: u64| {};
    let issues = scan::validate_stream(&path, &cancel, &mut progress).unwrap();
    println!(
        "流式校验：{} 个错误，用时 {} ms（不整份载入）",
        issues.len(),
        t.elapsed().as_millis()
    );
}
