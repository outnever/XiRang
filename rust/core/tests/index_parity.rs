//! 正确性对拍：工作区台账 / 每文件侧车 / 整份载入 三种后端，
//! 在同一批编号上的四类查询（点查 / 孩子 / 反向 / 整树）必须逐条一致。

use std::collections::HashSet;
use std::path::PathBuf;

use xirang_core::codec::Uuid;
use xirang_core::index::Backend as _;
use xirang_core::{fixture, index, wsidx};

fn tmp_dir(tag: &str) -> PathBuf {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let d = std::env::temp_dir().join(format!("xr-parity-{tag}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// 对一批编号取快照：每项 = (点查结果, 孩子, 反向来源, 整树)。
fn snapshot(
    b: &mut dyn index::Backend,
    ids: &[Uuid],
) -> Vec<(Vec<Uuid>, Vec<Uuid>, Vec<Uuid>, Vec<Uuid>)> {
    ids.iter()
        .map(|id| {
            let loc = sort(b.locate(*id).into_iter().map(|(_, n)| n.id));
            let kids = sort(b.children(*id).into_iter().map(|(_, n)| n.id));
            let rev = sort(b.references(*id).into_iter().map(|(_, s)| s));
            let sub = sort(b.subtree(*id).into_iter().map(|(_, n)| n.id));
            (loc, kids, rev, sub)
        })
        .collect()
}

fn sort(it: impl Iterator<Item = Uuid>) -> Vec<Uuid> {
    // Uuid 没有 Ord，按字节排序
    let set: HashSet<[u8; 16]> = it.map(|u| u.0).collect();
    let mut v: Vec<[u8; 16]> = set.into_iter().collect();
    v.sort();
    v.into_iter().map(Uuid).collect()
}

#[test]
fn three_backends_agree_on_four_query_kinds() {
    let dir = tmp_dir("agree");
    let spec = fixture::FixtureSpec {
        kind: fixture::FixtureKind::Multi,
        files: 3,
        entries_per_file: 40,
        inherit_ratio: 1,
        seed: 42,
        name: "parity".into(),
    };
    let info = fixture::build(&dir, &spec).unwrap();
    assert!(info.nodes > 100, "夹具应产出足够的节点：{}", info.nodes);

    // 建工作区台账
    let st = wsidx::rebuild(&dir, &[]).unwrap();
    assert_eq!(st.files, info.files.len());
    assert!(st.loc > 0 && st.rel > 0 && st.rev > 0, "{st:?}");

    let paths: Vec<String> =
        info.files.iter().map(|p| p.to_string_lossy().into_owned()).collect();

    // 抽样：词条、词形、继承树、被引用节点
    let mut ids: Vec<Uuid> = Vec::new();
    ids.extend(info.entry_ids.iter().take(20).copied());
    ids.extend(info.word_ids.iter().take(20).copied());
    ids.extend(info.inherit_ids.iter().take(20).copied());
    ids.extend(info.referenced_ids.iter().take(20).copied());

    let mut ws_backend = index::WorkspaceBackend::open(&dir).unwrap();
    let mut side = index::SidecarBackend::open(&paths).unwrap();
    let mut mem = index::MemoryBackend::load(&paths).unwrap();

    let a = snapshot(&mut ws_backend, &ids);
    let b = snapshot(&mut side, &ids);
    let c = snapshot(&mut mem, &ids);
    assert_eq!(a, b, "工作区台账与侧车结果不一致");
    assert_eq!(a, c, "工作区台账与整份载入结果不一致");

    // 有内容的断言：至少有一条命中/孩子/反向/整树
    assert!(a.iter().any(|(loc, _, _, _)| !loc.is_empty()));
    assert!(a.iter().any(|(_, kids, _, _)| !kids.is_empty()));
    assert!(a.iter().any(|(_, _, rev, _)| !rev.is_empty()), "反向索引应有结果");
    assert!(a.iter().any(|(_, _, _, sub)| sub.len() > 1), "整树应包含子孙");

    std::fs::remove_dir_all(&dir).ok();
    std::fs::remove_dir_all(index_dir_of(&dir)).ok();
}

fn index_dir_of(ws_root: &std::path::Path) -> PathBuf {
    wsidx::index_dir(ws_root)
}

#[test]
fn index_covers_every_node_of_every_file() {
    let dir = tmp_dir("cover");
    let spec = fixture::FixtureSpec {
        kind: fixture::FixtureKind::Multi,
        files: 2,
        entries_per_file: 5,
        inherit_ratio: 1,
        seed: 11,
        name: "cover".into(),
    };
    let info = fixture::build(&dir, &spec).unwrap();
    wsidx::rebuild(&dir, &[]).unwrap();
    let paths: Vec<String> =
        info.files.iter().map(|p| p.to_string_lossy().into_owned()).collect();
    let mut ws_backend = index::WorkspaceBackend::open(&dir).unwrap();
    let mut mem = index::MemoryBackend::load(&paths).unwrap();

    let mut missing = Vec::new();
    let mut total = 0usize;
    for p in &info.files {
        let store = xirang_core::tree::Store::load(p).unwrap();
        for n in store.nodes() {
            total += 1;
            let a = ws_backend.locate(n.id);
            let b = mem.locate(n.id);
            if a.len() != b.len() || a.is_empty() {
                missing.push(format!(
                    "{} {}（台账 {} 份 / 载入 {} 份）",
                    n.id,
                    n.name,
                    a.len(),
                    b.len()
                ));
            }
        }
    }
    assert!(missing.is_empty(), "共 {total} 个节点，以下没被索引覆盖：{missing:?}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn log_overlay_matches_after_incremental_write() {
    let dir = tmp_dir("log");
    let spec = fixture::FixtureSpec {
        kind: fixture::FixtureKind::Multi,
        files: 2,
        entries_per_file: 30,
        inherit_ratio: 1,
        seed: 7,
        name: "logtest".into(),
    };
    let info = fixture::build(&dir, &spec).unwrap();
    wsidx::rebuild(&dir, &[]).unwrap();

    let paths: Vec<String> =
        info.files.iter().map(|p| p.to_string_lossy().into_owned()).collect();

    // 改一个词形的值，然后只追加日志（不压实）
    let target = info.word_ids[0];
    let mut store = xirang_core::tree::Store::load(&info.files[0]).unwrap();
    store.update(target, xirang_core::codec::Value::Text("改过的词形".into())).unwrap();
    store.save(&info.files[0]).unwrap();
    wsidx::append_file(&dir, &info.files[0]).unwrap();

    let mut ws_backend = index::WorkspaceBackend::open(&dir).unwrap();
    let mut mem = index::MemoryBackend::load(&paths).unwrap();
    let ids = vec![target];
    assert_eq!(snapshot(&mut ws_backend, &ids), snapshot(&mut mem, &ids));

    // 压实之后仍然一致
    wsidx::compact(&dir).unwrap();
    let mut ws_backend = index::WorkspaceBackend::open(&dir).unwrap();
    let mut mem = index::MemoryBackend::load(&paths).unwrap();
    assert_eq!(snapshot(&mut ws_backend, &ids), snapshot(&mut mem, &ids));

    std::fs::remove_dir_all(&dir).ok();
}
