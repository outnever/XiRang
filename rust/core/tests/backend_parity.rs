//! 两种索引后端（工作区台账 / 侧车）**对拍**：同一份数据，查询结果必须逐条一致。
//!
//! 这是「GUI 与 CLI 共用同一份索引」的底线：两条实现路径给出同样的答案，
//! 界面才不会因为换了索引模式而看到不一样的东西。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use xirang_core::codec::{Node, Uuid, Value};
use xirang_core::index::{self as xindex, Backend, Budget, SidecarBackend, WorkspaceBackend};
use xirang_core::tree::Store;

fn tmp_dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("xr_parity_{name}_{}", Uuid::random_v4()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// 造两份互相引用的文件：
/// a.xirang：根「甲」→ 子「指向乙」(引用 b 的「被指向」) → 子「甲的内层」
/// b.xirang：根「乙」→ 子「被指向」→ 子「回指甲的指向乙」(引用 a 的节点)
fn make_files(dir: &Path) -> (PathBuf, PathBuf, Vec<Uuid>) {
    let a_path = dir.join("a.xirang");
    let b_path = dir.join("b.xirang");

    let mut b = Store::new();
    let root_b = b.create(None, "乙", Value::Empty, false).id;
    let target = b.create(Some(root_b), "被指向", Value::Empty, false).id;
    b.save(&b_path).unwrap();

    let mut a = Store::new();
    let root_a = a.create(None, "甲", Value::Empty, false).id;
    let link = a.create(Some(root_a), "指向乙", Value::Reference(target), false).id;
    a.create(Some(link), "甲的内层", Value::Text("x".into()), false);
    a.save(&a_path).unwrap();

    (a_path, b_path, vec![root_a, root_b])
}

fn ids_of(nodes: &[(String, Node)]) -> BTreeSet<String> {
    nodes.iter().map(|(_, n)| n.id.to_string()).collect()
}

fn roots_of(b: &mut dyn Backend) -> BTreeSet<String> {
    b.roots().iter().map(|u| u.to_string()).collect()
}

#[test]
fn sidecar_and_workspace_agree_on_the_same_data() {
    let dir = tmp_dir("agree");
    let (a_path, b_path, _) = make_files(&dir);
    let paths = vec![
        a_path.display().to_string(),
        b_path.display().to_string(),
    ];

    // 两种索引各建一次（都是缓存，删了会重建）
    for p in &paths {
        xindex::rebuild(Path::new(p)).unwrap();
    }
    let ws_root = xirang_core::wsidx::workspace_root(Path::new(&paths[0]));
    xirang_core::wsidx::rebuild(&ws_root, &[a_path.clone(), b_path.clone()]).unwrap();

    let mut side = SidecarBackend::open(&paths).unwrap();
    let mut work = WorkspaceBackend::open(&ws_root).unwrap();

    // 顶层根：两边一致
    let r_side = roots_of(&mut side);
    let r_work = roots_of(&mut work);
    assert!(!r_side.is_empty(), "侧车应当能列出顶层根");
    assert_eq!(r_side, r_work, "两种索引模式列出的顶层根必须一致");

    // 孩子 / 孩子数 / 邻居：对每个根都比一遍
    for root in &r_side {
        let id = Uuid::parse(root).unwrap();
        let c_side = ids_of(&side.children(id));
        let c_work = ids_of(&work.children(id));
        assert_eq!(c_side, c_work, "根 {root} 的孩子两边必须一致");
        assert_eq!(
            side.child_count(id),
            work.child_count(id),
            "孩子数量两边必须一致"
        );
        let n_side = side.neighbors(id, 1, Budget::default());
        let n_work = work.neighbors(id, 1, Budget::default());
        let es: BTreeSet<String> = n_side
            .edges
            .iter()
            .map(|(a, b)| format!("{a}->{b}"))
            .collect();
        let ew: BTreeSet<String> = n_work
            .edges
            .iter()
            .map(|(a, b)| format!("{a}->{b}"))
            .collect();
        assert_eq!(es, ew, "根 {root} 的一跳邻域边两边必须一致");
    }
    let should_remove = !std::env::var("XR_KEEP_TMP").is_ok();
    if should_remove { let _ = std::fs::remove_dir_all(&dir); }
}

#[test]
fn budget_truncates_and_edges_among_only_returns_inside() {
    let dir = tmp_dir("budget");
    let (a_path, b_path, _) = make_files(&dir);
    let paths = vec![a_path.display().to_string(), b_path.display().to_string()];
    for p in &paths {
        xindex::rebuild(Path::new(p)).unwrap();
    }
    let ws_root = xirang_core::wsidx::workspace_root(Path::new(&paths[0]));
    xirang_core::wsidx::rebuild(&ws_root, &[a_path.clone(), b_path.clone()]).unwrap();

    for backend in [
        Box::new(SidecarBackend::open(&paths).unwrap()) as Box<dyn Backend>,
        Box::new(WorkspaceBackend::open(&ws_root).unwrap()) as Box<dyn Backend>,
    ] {
        let mut b = backend;
        let roots = b.roots();
        let root = roots[0];
        // 从根的孩子里挑一个"带引用"的节点（保证向外扩一定有事可做）
        let kids = b.children(root);
        let hub = kids
            .iter()
            .find(|(_, n)| matches!(n.value, Value::Reference(_)))
            .map(|(_, n)| n.id)
            .unwrap_or(root);
        // 极小预算：必须截断并回报，而不是无限扩散
        let tight = Budget {
            max_nodes: 1,
            max_edges: 1,
        };
        let n = b.neighbors(hub, 3, tight);
        assert!(n.truncated, "预算触顶必须回报截断（{}）", b.kind());
        assert!(n.nodes.len() <= 2, "截断后节点数受控（{}）", b.kind());

        // 集合内边：只给一个节点时，指向集合外的边不能出现
        let (edges, _) = b.edges_among(&[hub], Budget::default());
        assert!(
            edges.iter().all(|(a, t)| *a == hub && *t == hub),
            "{}：edges_among 只能返回集合内的边",
            b.kind()
        );
    }
    let should_remove = !std::env::var("XR_KEEP_TMP").is_ok();
    if should_remove { let _ = std::fs::remove_dir_all(&dir); }
}

#[test]
fn queries_do_not_write_anything() {
    let dir = tmp_dir("readonly");
    let (a_path, b_path, _) = make_files(&dir);
    let paths = vec![a_path.display().to_string(), b_path.display().to_string()];
    for p in &paths {
        xindex::rebuild(Path::new(p)).unwrap();
    }
    let ws_root = xirang_core::wsidx::workspace_root(Path::new(&paths[0]));
    xirang_core::wsidx::rebuild(&ws_root, &[a_path.clone(), b_path.clone()]).unwrap();

    let stamp = |p: &Path| {
        let m = std::fs::metadata(p).unwrap();
        (m.len(), m.modified().unwrap())
    };
    let before = (
        stamp(&a_path),
        stamp(&b_path),
        stamp(&xirang_core::index::sidecar_path(&a_path)),
    );

    let mut b = WorkspaceBackend::open(&ws_root).unwrap();
    let roots = b.roots();
    for r in &roots {
        let _ = b.children(*r);
        let _ = b.child_count(*r);
        let _ = b.neighbors(*r, 1, Budget::default());
    }
    let _ = b.edges_among(&roots, Budget::default());

    let after = (
        stamp(&a_path),
        stamp(&b_path),
        stamp(&xirang_core::index::sidecar_path(&a_path)),
    );
    assert_eq!(before, after, "查询不得改动数据文件或索引文件");
    let should_remove = !std::env::var("XR_KEEP_TMP").is_ok();
    if should_remove { let _ = std::fs::remove_dir_all(&dir); }
}
