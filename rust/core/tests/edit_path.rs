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

// ============================================================================
// 批量提交（`edit::apply_batch`）
// ============================================================================

/// 建一个有一批普通节点的文件：根 → 子 0..n-1（每个子节点都真存在）。
fn make_wide_file(dir: &Path, name: &str, n: usize) -> (PathBuf, Vec<Uuid>) {
    let path = dir.join(name);
    let mut s = Store::new();
    let root = s.create(None, "根", Value::Empty, false).id;
    let mut ids = Vec::new();
    for i in 0..n {
        ids.push(s.create(Some(root), &format!("词{i}"), Value::Text("旧".into()), false).id);
    }
    s.save(&path).unwrap();
    (path, ids)
}

/// 批量：同一编号改多次只写一条最终记录、只留一次痕；台账一次跟上。
#[test]
fn batch_folds_repeats_and_registers_once() {
    let dir = tmp_dir("batch");
    let (path, ids) = make_wide_file(&dir, "批量.xirang", 50);
    wsidx::rebuild(&dir, &[]).unwrap();

    let mut ops = Vec::new();
    for (i, id) in ids.iter().enumerate() {
        ops.push(edit::BatchOp::Set { id: *id, value: Value::Text(format!("新{i}")) });
        if i % 5 == 0 {
            // 同一个编号再来一次：应当以最后一次为准，而且只多一条记录
            ops.push(edit::BatchOp::Rename { id: *id, name: format!("改{i}") });
        }
    }
    let out = edit::apply_batch(&path, &ops, false, false, false, false).unwrap();
    assert_eq!(out.ops, ops.len());
    assert_eq!(out.changed, ids.len(), "同一编号改多次只算一个");
    // 不留痕：每个改动只追加一条记录，外加一条协议声明
    assert_eq!(out.appended, ids.len() + 1, "不留痕时每条改动只写一条记录");

    let view = tree::Store::load_view(&path).unwrap();
    for (i, id) in ids.iter().enumerate() {
        let n = view.get(*id).unwrap();
        assert_eq!(n.value, Value::Text(format!("新{i}")));
        if i % 5 == 0 {
            assert_eq!(n.name, format!("改{i}"), "改名要叠在改值之后");
        }
    }
    assert_eq!(
        view.nodes().iter().filter(|n| n.name == "@protocol").count(),
        1,
        "协议声明只补一条"
    );
    let st = wsidx::files_status(&dir).unwrap();
    assert!(st.iter().all(|f| f.fresh), "批量之后台账要新鲜：{st:?}");

    std::fs::remove_dir_all(&dir).ok();
    std::fs::remove_dir_all(wsidx::index_dir(&dir)).ok();
}

/// 批量是「整批不动」：任一条不合法，一个字节都不该变。
#[test]
fn batch_is_all_or_nothing() {
    let dir = tmp_dir("batchbad");
    let (path, ids) = make_wide_file(&dir, "整批.xirang", 20);
    wsidx::rebuild(&dir, &[]).unwrap();
    let before = std::fs::read(&path).unwrap();

    let ops = vec![
        edit::BatchOp::Set { id: ids[0], value: Value::Text("好".into()) },
        edit::BatchOp::Set { id: ids[1], value: Value::Text("也好".into()) },
        // 这一条故意指一个不存在的编号
        edit::BatchOp::Set {
            id: Uuid::random_v4(),
            value: Value::Text("坏".into()),
        },
    ];
    assert!(edit::apply_batch(&path, &ops, false, false, false, false).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), before, "失败时文件必须一个字节不变");
    assert_eq!(
        tree::Store::load_view(&path).unwrap().get(ids[0]).unwrap().value,
        Value::Text("旧".into()),
        "前面那几条也不许落地"
    );

    // 预演同样不写
    assert!(edit::apply_batch(&path, &ops[..2], false, false, true, false).is_ok());
    assert_eq!(std::fs::read(&path).unwrap(), before, "预演不写文件");

    std::fs::remove_dir_all(&dir).ok();
    std::fs::remove_dir_all(wsidx::index_dir(&dir)).ok();
}

/// 批量也守模板定义的护栏（`force` 才放行）。
#[test]
fn batch_respects_template_guard() {
    let dir = tmp_dir("batchtpl");
    let path = dir.join("模板.xirang");
    let mut s = Store::new();
    let tpl = s.create(None, "词条", Value::Empty, false).id;
    s.create(Some(tpl), "@模板", Value::Empty, false);
    let field = s.create(Some(tpl), "词形", Value::Empty, false).id;
    s.save(&path).unwrap();
    wsidx::rebuild(&dir, &[]).unwrap();

    let ops = vec![edit::BatchOp::Set { id: field, value: Value::Text("抢改".into()) }];
    let err = edit::apply_batch(&path, &ops, false, false, false, false).unwrap_err();
    assert!(err.contains("模板定义"), "应当被模板护栏拦下：{err}");
    // 显式强制才放行
    assert!(edit::apply_batch(&path, &ops, false, true, false, false).is_ok());

    std::fs::remove_dir_all(&dir).ok();
    std::fs::remove_dir_all(wsidx::index_dir(&dir)).ok();
}

/// 文件还没登记过（典型场景：把词库复制到干净目录）→ 就地整份登记，别卡住流程。
#[test]
fn batch_auto_registers_an_unregistered_file() {
    let dir = tmp_dir("batchreg");
    let src = tmp_dir("batchregsrc");
    let (from, ids) = make_wide_file(&src, "原件.xirang", 10);
    let path = dir.join("副本.xirang");
    std::fs::copy(&from, &path).unwrap(); // 复制出来的文件天然没登记

    let ops = vec![edit::BatchOp::Set { id: ids[0], value: Value::Text("改过".into()) }];
    let out = edit::apply_batch(&path, &ops, false, false, false, false)
        .expect("没登记过也要能跑（就地整份登记）");
    assert_eq!(out.changed, 1);
    assert_eq!(
        tree::Store::load_view(&path).unwrap().get(ids[0]).unwrap().value,
        Value::Text("改过".into())
    );
    let st = wsidx::files_status(&dir).unwrap();
    assert!(st.iter().any(|f| f.fresh), "改完台账里应当有这个文件：{st:?}");

    for d in [&dir, &src] {
        std::fs::remove_dir_all(d).ok();
        std::fs::remove_dir_all(wsidx::index_dir(d)).ok();
    }
}

/// 引用目标不存在 → 整批不动；`--allow-missing-target`（`allow_missing_target`）才放行。
#[test]
fn batch_refuses_missing_reference_target() {
    let dir = tmp_dir("batchtarget");
    let (path, ids) = make_wide_file(&dir, "目标.xirang", 5);
    wsidx::rebuild(&dir, &[]).unwrap();
    let before = std::fs::read(&path).unwrap();

    let ghost = Uuid::random_v4();
    let ops = vec![edit::BatchOp::Link { id: ids[0], to: ghost }];
    let err = edit::apply_batch(&path, &ops, false, false, false, false).unwrap_err();
    assert!(err.contains("引用目标"), "应当拦下悬空引用：{err}");
    assert_eq!(std::fs::read(&path).unwrap(), before, "拒绝时不许写文件");

    // 目标在同一个文件里就通过
    let ok = vec![edit::BatchOp::Link { id: ids[0], to: ids[1] }];
    edit::apply_batch(&path, &ok, false, false, false, false).unwrap();
    // 明确要写悬空引用时放行
    edit::apply_batch(&path, &ops, false, false, false, true).unwrap();

    std::fs::remove_dir_all(&dir).ok();
    std::fs::remove_dir_all(wsidx::index_dir(&dir)).ok();
}

/// `rm_subtree`：连子树一起清空（每个节点都置空，留痕打开时每个都能回滚）。
#[test]
fn batch_rm_subtree_empties_every_node_in_the_subtree() {
    let dir = tmp_dir("batchrm");
    let path = dir.join("子树.xirang");
    let mut s = Store::new();
    let root = s.create(None, "根", Value::Empty, false).id;
    let parent = s.create(Some(root), "义项", Value::Empty, false).id;
    let a = s.create(Some(parent), "释义", Value::Text("指人".into()), false).id;
    let b = s.create(Some(parent), "读音", Value::Text("rén".into()), false).id;
    let keep = s.create(Some(root), "别动", Value::Text("留着".into()), false).id;
    s.save(&path).unwrap();
    wsidx::rebuild(&dir, &[]).unwrap();

    let ops = vec![edit::BatchOp::RmSubtree { id: parent }];
    let out = edit::apply_batch(&path, &ops, true, false, false, false).unwrap();
    assert_eq!(out.changed, 3, "义项自己 + 两个孩子");

    let view = tree::Store::load_view(&path).unwrap();
    for id in [parent, a, b] {
        let n = view.get(id).expect("节点还在（追加写的语义：置空不物理删）");
        assert!(n.name.is_empty() && n.value == Value::Empty, "节点 {id} 应当被置空");
    }
    assert_eq!(
        view.get(keep).unwrap().value,
        Value::Text("留着".into()),
        "子树之外的不许动"
    );
    // 留痕打开时每个被清空的节点都有 @history（可以用 xr revert 逐个还原）
    for id in [parent, a, b] {
        let kids = view.children(view.get(id).unwrap());
        assert!(
            kids.iter().any(|k| k.name == "@history"),
            "节点 {id} 应当留下 @history"
        );
    }

    std::fs::remove_dir_all(&dir).ok();
    std::fs::remove_dir_all(wsidx::index_dir(&dir)).ok();
}
