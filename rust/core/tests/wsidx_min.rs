//! 工作区索引的最小常驻回归（不依赖任何测试脚手架）：
//! 1) 三种后端在四类查询上给出同样结果；
//! 2) 索引坏了要**大声报错**，绝不静默返回「查不到」。

use std::path::{Path, PathBuf};

use xirang_core::codec::{Node, Uuid, Value};
use xirang_core::index::Backend as _;
use xirang_core::{index, tree, wsidx};

fn tmp_dir(tag: &str) -> PathBuf {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let d = std::env::temp_dir().join(format!("xr-wsidx-{tag}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn node(id: Uuid, parent: Option<Uuid>, name: &str, value: Value) -> Node {
    Node { id, parent, name: name.to_string(), value }
}

/// 造两棵互相引用的最小树：a/甲 指向 b 里的「词义」。
fn build(dir: &Path) -> (Vec<String>, Uuid, Uuid) {
    let (a_root, a_ptr, b_root, b_target) =
        (Uuid::random_v4(), Uuid::random_v4(), Uuid::random_v4(), Uuid::random_v4());

    let mut a = tree::Store::new();
    a.add(node(a_root, None, "甲", Value::Empty));
    a.add(node(a_ptr, Some(a_root), "指向", Value::Reference(b_target)));
    let pa = dir.join("a.xirang");
    a.save(&pa).unwrap();

    let mut b = tree::Store::new();
    b.add(node(b_root, None, "乙", Value::Empty));
    b.add(node(b_target, Some(b_root), "词义", Value::Text("目标".into())));
    let pb = dir.join("b.xirang");
    b.save(&pb).unwrap();

    (
        vec![pa.to_string_lossy().into_owned(), pb.to_string_lossy().into_owned()],
        a_root,
        b_target,
    )
}

fn ids(it: impl Iterator<Item = Uuid>) -> Vec<[u8; 16]> {
    let mut v: Vec<[u8; 16]> = it.map(|u| u.0).collect();
    v.sort();
    v
}

#[test]
fn workspace_backend_agrees_with_full_load() {
    let dir = tmp_dir("agree");
    let (paths, a_root, b_target) = build(&dir);
    wsidx::rebuild(&dir, &[]).unwrap();

    let mut ws = index::WorkspaceBackend::open(&dir).unwrap();
    let mut mem = index::MemoryBackend::load(&paths).unwrap();
    for id in [a_root, b_target] {
        assert_eq!(
            ids(ws.locate(id).into_iter().map(|(_, n)| n.id)),
            ids(mem.locate(id).into_iter().map(|(_, n)| n.id)),
            "点查结果应与整份载入一致"
        );
        assert_eq!(
            ids(ws.children(id).into_iter().map(|(_, n)| n.id)),
            ids(mem.children(id).into_iter().map(|(_, n)| n.id))
        );
        assert_eq!(
            ids(ws.references(id).into_iter().map(|(_, s)| s)),
            ids(mem.references(id).into_iter().map(|(_, s)| s))
        );
        assert_eq!(
            ids(ws.subtree(id).into_iter().map(|(_, n)| n.id)),
            ids(mem.subtree(id).into_iter().map(|(_, n)| n.id))
        );
    }
    // 跨文件引用：甲 里的「指向」应指向 b 的节点
    assert_eq!(ws.references(b_target).len(), 1, "反向索引应看到跨文件的那条引用");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn broken_index_is_loud_and_falls_back() {
    let dir = tmp_dir("loud");
    let (paths, _a_root, b_target) = build(&dir);
    wsidx::rebuild(&dir, &[]).unwrap();

    // 删掉一个块文件 → 打开索引必须报 F012
    let idx = wsidx::index_dir(&dir);
    let blk = std::fs::read_dir(&idx)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.to_string_lossy().ends_with(".blk"))
        .expect("应有块文件");
    std::fs::remove_file(&blk).unwrap();
    let err = wsidx::Reader::open(&dir).err().expect("块缺失应报错");
    assert!(err.contains("F012"), "错误应带 F012：{err}");

    // 走索引的入口应回退到整份载入，并把原因说出来（不静默给空结果）
    let mut ws = index::LazyWorkspace::from_paths(&paths).unwrap();
    assert_eq!(ws.backend_kind(), "memory");
    assert!(ws.fallback_reason().unwrap().contains("F012"));
    assert!(!ws.node_views(b_target).is_empty(), "回退之后仍要查得到");
    std::fs::remove_dir_all(&dir).ok();
}
