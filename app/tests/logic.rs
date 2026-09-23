//! 桌面端非界面部分的测试：懒加载摊平、展开徽标、两种布局、追加编辑与撤销。

use std::collections::HashSet;
use std::path::PathBuf;

use xirang_app::edit;
use xirang_app::lazy::Doc;
use xirang_app::view::{flatten, Layout};
use xirang_core::codec::{Node, Uuid, Value};
use xirang_core::index::sidecar_path;
use xirang_core::tree::{self, Store};

fn tmp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("xr_app_{name}_{}.xirang", Uuid::random_v4()))
}

/// 造一个小文件：根（词条）→ 词形 / 词义 → 01 → 释义。
fn sample(path: &std::path::Path) -> (Uuid, Uuid, Uuid, Uuid) {
    let mut s = Store::new();
    let root = s.create(None, "灯", Value::Empty, false).id;
    let form = s.create(Some(root), "词形", Value::Text("灯".into()), false).id;
    let sense = s.create(Some(root), "词义", Value::Empty, false).id;
    let first = s.create(Some(sense), "01", Value::Empty, false).id;
    s.create(Some(first), "释义", Value::Text("照明器具".into()), false);
    s.save(path).unwrap();
    (root, form, sense, first)
}

fn cleanup(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(sidecar_path(path));
}

#[test]
fn flatten_shows_only_expanded_rows_with_child_badges() {
    let p = tmp("flatten");
    let (root, form, sense, first) = sample(&p);
    let mut doc = Doc::open(&p).unwrap();

    // 只展开根：应当看到根 + 两个孩子；「词义」有 1 个孩子，但没展开
    let mut expanded = HashSet::new();
    expanded.insert(root);
    let rows = flatten(&mut doc, &expanded, true, 1000);
    assert_eq!(rows.len(), 3, "根 + 词形 + 词义");
    assert_eq!(rows[0].name, "灯");
    assert!(rows[0].is_root);
    let sense_row = rows.iter().find(|r| r.id == sense).unwrap();
    assert_eq!(sense_row.child_count, 1);
    assert!(!sense_row.expanded, "没展开");
    assert_eq!(sense_row.twisty(), "▸ 1", "未展开时显示孩子数量徽标");
    assert!(!rows.iter().any(|r| r.id == first), "未展开的孩子不出现在行里");

    // 展开「词义」后出现 01；再展开 01 出现「释义」
    expanded.insert(sense);
    expanded.insert(first);
    let rows = flatten(&mut doc, &expanded, true, 1000);
    assert_eq!(rows.len(), 5);
    assert_eq!(rows.iter().find(|r| r.id == form).unwrap().twisty(), "·", "叶子");
    let root_row = rows.iter().find(|r| r.id == root).unwrap();
    assert_eq!(root_row.twisty(), "▾", "已展开");

    // CLI 风的引导线：根没有前缀，第二层用 ├─ / └─
    let guides: Vec<String> = rows
        .iter()
        .filter(|r| r.depth == 1)
        .map(|r| r.guide())
        .collect();
    assert_eq!(guides, vec!["├─ ".to_string(), "└─ ".to_string()]);

    // 两种布局给出不同的缩进
    let row = &rows[1];
    assert!(row.indent(Layout::Layered) > row.indent(Layout::Indent));
    cleanup(&p);
}

#[test]
fn flatten_respects_aux_toggle_and_budget() {
    let p = tmp("aux");
    let (root, _, _, _) = sample(&p);
    let mut s = Store::load_view(&p).unwrap();
    s.create(Some(root), "@note", Value::Text("注释".into()), false);
    s.save(&p).unwrap();

    let mut doc = Doc::open(&p).unwrap();
    let mut expanded = HashSet::new();
    expanded.insert(root);

    let with_aux = flatten(&mut doc, &expanded, true, 1000);
    let without_aux = flatten(&mut doc, &expanded, false, 1000);
    assert_eq!(with_aux.len(), 4, "含 @note");
    assert_eq!(without_aux.len(), 3, "隐藏辅助节点后少一行");

    let capped = flatten(&mut doc, &expanded, true, 2);
    assert_eq!(capped.len(), 2, "预算封顶");
    cleanup(&p);
}

#[test]
fn edit_value_rename_create_delete_with_undo_redo() {
    let p = tmp("edit");
    let (root, form, _, _) = sample(&p);
    let mut doc = Doc::open(&p).unwrap();
    let mut editor = edit::Editor::open(&p).unwrap();

    // 改值：即时落盘，重新打开就能看到
    let node = doc.node(form).unwrap();
    editor
        .apply(edit::set_value(&node, Value::Text("灯（改）".into()), Some(root)))
        .unwrap();
    doc.reload().unwrap();
    assert_eq!(doc.node(form).unwrap().value, Value::Text("灯（改）".into()));

    // 首次编辑后，根下自动挂了 append-v1 声明
    let view = Store::load_view(&p).unwrap();
    assert!(view.declares_protocol(root, tree::PROTOCOL_APPEND));

    // 撤销 → 回到旧值；重做 → 又是新值
    assert!(editor.undo().unwrap());
    doc.reload().unwrap();
    assert_eq!(doc.node(form).unwrap().value, Value::Text("灯".into()));
    assert!(editor.redo().unwrap());
    doc.reload().unwrap();
    assert_eq!(doc.node(form).unwrap().value, Value::Text("灯（改）".into()));

    // 改名
    let node = doc.node(form).unwrap();
    editor
        .apply(edit::rename(&node, "词形2".into(), Some(root)))
        .unwrap();
    doc.reload().unwrap();
    assert_eq!(doc.node(form).unwrap().name, "词形2");

    // 新增子节点
    editor
        .apply(edit::create(
            Some(form),
            "拼音".into(),
            Value::Text("dēng".into()),
            Some(root),
        ))
        .unwrap();
    doc.reload().unwrap();
    let kids = doc.children(form);
    assert_eq!(kids.len(), 1);
    assert_eq!(doc.node(kids[0]).unwrap().name, "拼音");

    // 删除 = 置空槽位（编号还在）
    let kid = doc.node(kids[0]).unwrap();
    editor.apply(edit::delete(&kid, Some(root))).unwrap();
    doc.reload().unwrap();
    let kid = doc.node(kids[0]).unwrap();
    assert!(kid.name.is_empty());
    assert_eq!(kid.value, Value::Empty);

    // 撤销删除 → 名字和值回来
    editor.undo().unwrap();
    doc.reload().unwrap();
    assert_eq!(doc.node(kids[0]).unwrap().name, "拼音");

    // 文件里确实保留了历史记录（记录数多于折叠后的编号数）
    let raw = Store::load(&p).unwrap();
    let folded = Store::load_view(&p).unwrap();
    assert!(
        raw.len() > folded.len(),
        "追加日志保留了历史：{} 条记录 → {} 个编号",
        raw.len(),
        folded.len()
    );
    cleanup(&p);
}

#[test]
fn lazy_reads_only_what_is_needed() {
    let p = tmp("lazy");
    sample(&p);
    let mut doc = Doc::open(&p).unwrap();
    assert_eq!(doc.reads, 0, "打开时不读节点");
    assert_eq!(doc.node_count(), 5, "根 + 词形 + 词义 + 01 + 释义");
    let roots = doc.roots();
    assert_eq!(roots.len(), 1);
    let _ = doc.node(roots[0]);
    assert_eq!(doc.reads, 1, "只读了一个节点");
    let _ = doc.node(roots[0]);
    assert_eq!(doc.hits, 1, "第二次命中缓存");
    cleanup(&p);
}

#[test]
fn built_file_still_readable_by_core() {
    // 兜底：桌面端写的文件，CLI / 核心库按折叠视图读得回来
    let p = tmp("compat");
    let (root, form, _, _) = sample(&p);
    let mut editor = edit::Editor::open(&p).unwrap();
    let mut doc = Doc::open(&p).unwrap();
    let node: Node = doc.node(form).unwrap();
    editor
        .apply(edit::set_value(&node, Value::Int(42), Some(root)))
        .unwrap();

    let view = Store::load_view(&p).unwrap();
    assert_eq!(view.get(form).unwrap().value, Value::Int(42));
    assert!(xirang_core::validator::validate_view(&view).is_empty());
    cleanup(&p);
}
