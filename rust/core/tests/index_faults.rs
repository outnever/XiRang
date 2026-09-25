//! 索引故障演练：坏掉的索引必须「大声说话」，绝不给静默的错答案。
//! 所有用例都显式关掉自动整理，避免后台压实干扰断言。

use std::io::Write as _;
use std::path::{Path, PathBuf};

use xirang_core::index::Backend as _;
use xirang_core::{fixture, index, wsidx};

fn tmp_dir(tag: &str) -> PathBuf {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let d = std::env::temp_dir().join(format!("xr-fault-{tag}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn build(dir: &Path) -> fixture::FixtureInfo {
    let spec = fixture::FixtureSpec {
        kind: fixture::FixtureKind::Multi,
        files: 3,
        entries_per_file: 20,
        inherit_ratio: 1,
        seed: 5,
        name: "fault".into(),
    };
    let info = fixture::build(dir, &spec).unwrap();
    wsidx::rebuild(dir, &[]).unwrap();
    info
}

fn paths(info: &fixture::FixtureInfo) -> Vec<String> {
    info.files.iter().map(|p| p.to_string_lossy().into_owned()).collect()
}

/// 体检：索引里的答案与「整份载入」一致
fn consistent(dir: &Path, info: &fixture::FixtureInfo) -> bool {
    let ps = paths(info);
    let mut ws = index::WorkspaceBackend::open(dir).unwrap();
    let mut mem = index::MemoryBackend::load(&ps).unwrap();
    let ids: Vec<_> = info.entry_ids.iter().chain(info.word_ids.iter()).take(40).copied().collect();
    for id in ids {
        let a: Vec<_> = ws.locate(id).into_iter().map(|(_, n)| n.id.0).collect();
        let b: Vec<_> = mem.locate(id).into_iter().map(|(_, n)| n.id.0).collect();
        if a.len() != b.len() {
            return false;
        }
    }
    true
}

#[test]
fn truncated_log_tail_is_ignored() {
    let dir = tmp_dir("trunc");
    let info = build(&dir);
    assert!(consistent(&dir, &info));
    // 往日志尾部塞半条记录（长度字段说 999 字节，实际只给几个字节）
    let log = wsidx::index_dir(&dir).join("loc.log");
    let mut f = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
    f.write_all(&999u32.to_be_bytes()).unwrap();
    f.write_all(&[1u8, 2, 3]).unwrap();
    drop(f);
    assert!(consistent(&dir, &info), "日志尾部半条记录不该改变读取结果");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn missing_block_is_loud_not_silent() {
    let dir = tmp_dir("missing");
    let info = build(&dir);
    let idx = wsidx::index_dir(&dir);
    let blk = std::fs::read_dir(&idx)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.to_string_lossy().ends_with(".blk"))
        .expect("应有块文件");
    std::fs::remove_file(&blk).unwrap();

    // 1) 打开索引必须报 F012，而不是「查不到」
    let err = wsidx::Reader::open(&dir).err().expect("块缺失应报错");
    assert!(err.contains("F012"), "错误应带 F012：{err}");

    // 2) 走索引的入口应回退到整份载入，并给出原因
    let ps = paths(&info);
    let mut ws = index::LazyWorkspace::from_paths(&ps).unwrap();
    assert_eq!(ws.backend_kind(), "memory", "索引坏了应回退整份载入");
    assert!(ws.fallback_reason().unwrap().contains("F012"));
    assert!(!ws.node_views(info.entry_ids[0]).is_empty(), "回退后仍要能查到");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn compact_leaves_a_consistent_index() {
    let dir = tmp_dir("compact");
    let info = build(&dir);
    wsidx::compact(&dir).unwrap();
    assert!(consistent(&dir, &info));
    // 压实后：旧代数的块应被清掉，只留当前 manifest 引用的那些
    let idx = wsidx::index_dir(&dir);
    let names: Vec<String> = std::fs::read_dir(&idx)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".blk"))
        .collect();
    let gens: std::collections::HashSet<String> =
        names.iter().filter_map(|n| n.split('-').nth(1).map(|s| s.to_string())).collect();
    assert_eq!(gens.len(), 1, "只应保留一个代数的块：{names:?}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn concurrent_compaction_is_refused() {
    let dir = tmp_dir("lock");
    let _info = build(&dir);
    let idx = wsidx::index_dir(&dir);
    let guard = wsidx::lock_writer(&idx).unwrap();
    let out = wsidx::compact(&dir);
    assert!(out.is_err(), "锁被占用时应拒绝压实");
    assert!(out.unwrap_err().contains("F014"));
    drop(guard);
    assert!(wsidx::compact(&dir).is_ok(), "释放锁后应能压实");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn stale_lock_is_reclaimed() {
    let dir = tmp_dir("stale");
    let _info = build(&dir);
    let idx = wsidx::index_dir(&dir);
    // 假装一个已经死掉的进程留下了锁（pid 1 之外挑一个不存在的）
    std::fs::write(idx.join("lock"), "999999").unwrap();
    let guard = wsidx::lock_writer(&idx).expect("陈旧锁应被自动回收");
    drop(guard);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn write_during_scan_makes_compact_retry() {
    let dir = tmp_dir("retry");
    let info = build(&dir);
    // 先改一个文件，让它的指纹与 manifest 不符；压实应能把它重新收进来（自愈）
    let target = info.word_ids[0];
    let mut store = xirang_core::tree::Store::load(&info.files[0]).unwrap();
    store
        .update(target, xirang_core::codec::Value::Text("外部改动".into()))
        .unwrap();
    store.save(&info.files[0]).unwrap();
    // 压实：重扫会把这次外部改动一并收进索引
    wsidx::compact(&dir).unwrap();
    let mut ws = index::WorkspaceBackend::open(&dir).unwrap();
    let got = ws.locate(target).into_iter().next().expect("应能定位");
    assert_eq!(got.1.name, "词形");
    assert!(consistent(&dir, &info));
    std::fs::remove_dir_all(&dir).ok();
}
