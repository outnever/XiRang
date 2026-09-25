//! append-v1（修订v1）端到端测试：追加写 → 增量索引 → 折叠读 → 合并。

use std::path::PathBuf;

use xirang_core::codec::{Node, Uuid, Value};
use xirang_core::index::{
    build, compact_file, read_node_at, sidecar_path, AppendWriter, Sidecar,
};
use xirang_core::tree::{self, Store};


/// 本文件的测试验的是 **侧车后端** 上的 append-v1 增量（修订块 / 尾部残片）。
/// 主线把默认索引模式改成了「工作区台账（wsidx）」，所以这里显式锁到侧车模式。
fn sidecar_mode() {
    std::env::set_var("XIRANG_INDEX_MODE", "sidecar");
}

fn tmp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "xr_append_{name}_{}.xirang",
        Uuid::random_v4()
    ))
}

fn node(id: Uuid, parent: Option<Uuid>, name: &str, value: Value) -> Node {
    Node {
        id,
        parent,
        name: name.into(),
        value,
    }
}

fn marker(parent: Uuid) -> Node {
    node(
        Uuid::random_v4(),
        Some(parent),
        "@protocol",
        Value::Text(tree::PROTOCOL_APPEND.into()),
    )
}

#[test]
fn append_revision_is_visible_through_sidecar_and_view() {
    sidecar_mode();
    let p = tmp("visible");
    let mut s = Store::new();
    let root = s.create(None, "根", Value::Empty, false).id;
    let child = s.create(Some(root), "词形", Value::Text("灯".into()), false).id;
    s.save(&p).unwrap();

    let (mut w, _) = AppendWriter::open(&p).unwrap();
    w.append_node(&node(child, Some(root), "词形", Value::Text("灯（改）".into())))
        .unwrap();
    w.append_node(&marker(root)).unwrap();
    let rep = w.sync().unwrap();
    assert_eq!(rep.new_records, 2);
    assert_eq!(rep.truncated_bytes, 0);

    let mut sc = Sidecar::open_for(&p).unwrap();
    let (rel, len) = sc.find_node_loc(child).unwrap().unwrap();
    let got = read_node_at(&p, sc.node_data_start, rel, len).unwrap();
    assert_eq!(got.value, Value::Text("灯（改）".into()), "后写覆盖取最后一条");
    assert_eq!(sc.find_assign(child).unwrap(), Some(root));
    assert_eq!(sc.find_children(root).unwrap().len(), 2, "孩子不重复");
    assert_eq!(sc.node_count, 2, "基础块保持旧内容");
    assert_eq!(sc.rev_count, 2, "新记录进修订块");

    let view = Store::load_view(&p).unwrap();
    assert_eq!(view.len(), 3);
    assert_eq!(view.get(child).unwrap().value, Value::Text("灯（改）".into()));
    assert!(view.declares_protocol(root, tree::PROTOCOL_APPEND));

    let _ = std::fs::remove_file(&p);
    let _ = std::fs::remove_file(sidecar_path(&p));
}

#[test]
fn incremental_index_matches_full_rebuild() {
    sidecar_mode();
    let p = tmp("incremental");
    let mut s = Store::new();
    let a = s.create(None, "甲", Value::Empty, false).id;
    let b = s.create(None, "乙", Value::Empty, false).id;
    let c = s.create(Some(a), "丙", Value::Text("x".into()), false).id;
    s.save(&p).unwrap();

    let (mut w, _) = AppendWriter::open(&p).unwrap();
    w.append_node(&node(c, Some(b), "丙", Value::Text("y".into())))
        .unwrap(); // 搬家 + 改值
    let d = Uuid::random_v4();
    w.append_node(&node(d, Some(a), "丁", Value::Empty)).unwrap(); // 新孩子
    w.sync().unwrap();

    let mut inc = Sidecar::open_for(&p).unwrap();
    let data = std::fs::read(&p).unwrap();
    let rebuilt = build(&data).unwrap();

    let mut inc_kids: Vec<Uuid> = inc
        .find_children(a)
        .unwrap()
        .into_iter()
        .map(|(id, _, _)| id)
        .collect();
    let mut exp_kids: Vec<Uuid> = rebuilt
        .children
        .get(&a)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|(id, _, _)| id)
        .collect();
    inc_kids.sort_by(|x, y| x.0.cmp(&y.0));
    exp_kids.sort_by(|x, y| x.0.cmp(&y.0));
    assert_eq!(inc_kids, exp_kids, "增量索引与整份重建给同样的孩子集合");
    assert_eq!(inc.find_assign(c).unwrap(), rebuilt.assign.get(&c).copied());
    assert_eq!(inc.find_assign(d).unwrap(), rebuilt.assign.get(&d).copied());
    assert!(inc
        .find_children(a)
        .unwrap()
        .iter()
        .any(|(id, _, _)| *id == d));

    let _ = std::fs::remove_file(&p);
    let _ = std::fs::remove_file(sidecar_path(&p));
}

#[test]
fn trailing_fragment_is_truncated() {
    sidecar_mode();
    let p = tmp("fragment");
    let mut s = Store::new();
    let root = s.create(None, "根", Value::Empty, false).id;
    s.create(Some(root), "甲", Value::Text("值".into()), false);
    s.save(&p).unwrap();
    let good = std::fs::metadata(&p).unwrap().len();

    // 模拟「追加写到一半崩了」
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        f.write_all(&[0u8; 11]).unwrap();
    }

    let (mut w, rep) = AppendWriter::open(&p).unwrap();
    assert_eq!(rep.truncated_bytes, 11, "报告残片字节数（F011）");
    assert_eq!(std::fs::metadata(&p).unwrap().len(), good, "残片被截断");

    w.append_node(&node(root, None, "根", Value::Text("新".into())))
        .unwrap();
    w.sync().unwrap();
    let view = Store::load_view(&p).unwrap();
    assert_eq!(view.get(root).unwrap().value, Value::Text("新".into()));

    let _ = std::fs::remove_file(&p);
    let _ = std::fs::remove_file(sidecar_path(&p));
}

#[test]
fn compact_file_folds_revisions_and_keeps_view() {
    sidecar_mode();
    let p = tmp("compact");
    let mut s = Store::new();
    let root = s.create(None, "根", Value::Empty, false).id;
    let child = s.create(Some(root), "词形", Value::Text("0".into()), false).id;
    s.save(&p).unwrap();

    let (mut w, _) = AppendWriter::open(&p).unwrap();
    for i in 1..=5 {
        w.append_node(&node(child, Some(root), "词形", Value::Text(format!("{i}"))))
            .unwrap();
    }
    w.sync().unwrap();
    let before = std::fs::metadata(&p).unwrap().len();
    assert!(Sidecar::open_for(&p).unwrap().rev_count >= 5);

    let (raw, folded) = compact_file(&p).unwrap();
    assert_eq!(raw, 7, "原始记录 = 2 基版 + 5 修订");
    assert_eq!(folded, 2, "折叠后只剩两个编号");
    assert!(std::fs::metadata(&p).unwrap().len() < before, "文件变小");

    let sc = Sidecar::open_for(&p).unwrap();
    assert_eq!(sc.rev_count, 0, "合并后索引没有修订条目");
    let view = Store::load_view(&p).unwrap();
    assert_eq!(view.get(child).unwrap().value, Value::Text("5".into()));

    let _ = std::fs::remove_file(&p);
    let _ = std::fs::remove_file(sidecar_path(&p));
}

/// `all_edges`：全局引用图的取数——边跟着「最后一条记录」走。
#[test]
fn all_edges_follows_last_write_wins() {
    sidecar_mode();
    let p = tmp("edges");
    let mut s = Store::new();
    let a = s.create(None, "甲", Value::Empty, false).id;
    let b = s.create(None, "乙", Value::Empty, false).id;
    let link = s.create(Some(a), "指向乙", Value::Reference(b), false).id;
    s.save(&p).unwrap();

    let mut sc = Sidecar::open_for(&p).unwrap();
    assert_eq!(sc.all_edges().unwrap(), vec![(link, b)]);

    // 改成空值 → 这条边应当消失
    let (mut w, _) = AppendWriter::open(&p).unwrap();
    w.append_node(&node(link, Some(a), "指向乙", Value::Empty))
        .unwrap();
    w.sync().unwrap();
    let mut sc = Sidecar::open_for(&p).unwrap();
    assert!(sc.all_edges().unwrap().is_empty(), "值改成空后不该还有边");

    // 新增一条引用 → 边出现
    let c = Uuid::random_v4();
    let (mut w, _) = AppendWriter::open(&p).unwrap();
    w.append_node(&node(c, Some(a), "指向甲", Value::Reference(a)))
        .unwrap();
    w.sync().unwrap();
    let mut sc = Sidecar::open_for(&p).unwrap();
    assert_eq!(sc.all_edges().unwrap(), vec![(c, a)]);

    let _ = std::fs::remove_file(&p);
    let _ = std::fs::remove_file(sidecar_path(&p));
}

/// 大文件实测（手动跑）：
/// `XIRANG_BIG=/path/big.xirang cargo test -p xirang-core --test append_v1 -- --ignored --nocapture`
#[test]
#[ignore]
fn bench_open_and_edit_on_big_file() {
    sidecar_mode();
    let path = match std::env::var("XIRANG_BIG") {
        Ok(p) => PathBuf::from(p),
        Err(_) => return,
    };

    let t = std::time::Instant::now();
    let mut sc = Sidecar::open_for(&path).unwrap();
    let open_ms = t.elapsed().as_millis();
    println!("冷开（读侧车索引，不载入节点）: {open_ms} ms");
    println!(
        "  索引: 基础节点 {} 条 + 修订 {} 条；已扫到源文件 {} 字节",
        sc.node_count, sc.rev_count, sc.indexed_upto
    );

    // 只读一个根（懒加载）：查目录 → 跳字节 → 解一棵子树
    let root_id = sc.find_root_any().unwrap().unwrap();
    let t = std::time::Instant::now();
    let (rel, len) = sc.find_node_loc(root_id).unwrap().unwrap();
    let n = read_node_at(&path, sc.node_data_start, rel, len).unwrap();
    println!("按下标取一个节点: {:?} 用时 {} µs", n.name, t.elapsed().as_micros());

    // 改一个词 = 追加一条修订记录
    let t = std::time::Instant::now();
    let (mut w, _) = AppendWriter::open(&path).unwrap();
    let appended = node(
        root_id,
        None,
        &n.name,
        Value::Text("bench".into()),
    );
    w.append_node(&appended).unwrap();
    let rep = w.sync().unwrap();
    println!(
        "改一个词（追加 {} 条记录 + 索引补扫）: {} ms",
        rep.new_records,
        t.elapsed().as_millis()
    );

    // 对照：整份载入
    let t = std::time::Instant::now();
    let full = Store::load(&path).unwrap();
    println!(
        "对照 · 整份载入 {} 个节点: {} ms",
        full.len(),
        t.elapsed().as_millis()
    );
}
