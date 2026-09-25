//! 索引管理命令的内核行为：files / update / drop / forget / unlock。

use std::path::{Path, PathBuf};

use xirang_core::index::Backend as _;
use xirang_core::{fixture, index, wsidx};

fn tmp_dir(tag: &str) -> PathBuf {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let d = std::env::temp_dir().join(format!("xr-admin-{tag}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn build(dir: &Path) -> fixture::FixtureInfo {
    let spec = fixture::FixtureSpec {
        kind: fixture::FixtureKind::Multi,
        files: 2,
        entries_per_file: 15,
        inherit_ratio: 1,
        seed: 9,
        name: "admin".into(),
    };
    let info = fixture::build(dir, &spec).unwrap();
    wsidx::rebuild(dir, &[]).unwrap();
    info
}

#[test]
fn files_flags_external_change_and_update_heals() {
    let dir = tmp_dir("heal");
    let info = build(&dir);
    assert!(wsidx::files_status(&dir).unwrap().iter().all(|f| f.fresh));

    // 绕过 CLI 直接改数据文件（模拟「外部改过」）
    let target = info.word_ids[0];
    let mut store = xirang_core::tree::Store::load(&info.files[0]).unwrap();
    store
        .update(target, xirang_core::codec::Value::Text("外部改动".into()))
        .unwrap();
    store.save(&info.files[0]).unwrap();

    let listed = wsidx::files_status(&dir).unwrap();
    assert!(listed.iter().any(|f| !f.fresh), "应标出指纹不符的文件");
    assert_eq!(listed.iter().filter(|f| !f.fresh).count(), 1);

    let (n, entries, _) = wsidx::update(&dir).unwrap();
    assert_eq!(n, 1, "只应更新被改动的那一个文件");
    assert!(entries > 0);
    assert!(wsidx::files_status(&dir).unwrap().iter().all(|f| f.fresh), "update 之后应全部同步");

    // 自愈之后查询仍与整份载入一致
    let mut ws = index::WorkspaceBackend::open(&dir).unwrap();
    let mut mem = index::MemoryBackend::load(&[info.files[0].to_string_lossy().into_owned()]).unwrap();
    let a: Vec<_> = ws.locate(target).into_iter().map(|(_, n)| n.id.0).collect();
    let b: Vec<_> = mem.locate(target).into_iter().map(|(_, n)| n.id.0).collect();
    assert_eq!(a, b);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn drop_removes_only_the_index_dir() {
    let dir = tmp_dir("drop");
    let info = build(&dir);
    let before: Vec<u64> = info.files.iter().map(|p| p.metadata().unwrap().len()).collect();
    let idx = wsidx::index_dir(&dir);
    assert!(idx.exists());

    let (bytes, files) = wsidx::drop_index(&dir).unwrap();
    assert!(bytes > 0 && files == info.files.len());
    assert!(!idx.exists(), "索引目录应被删除");
    let after: Vec<u64> = info.files.iter().map(|p| p.metadata().unwrap().len()).collect();
    assert_eq!(before, after, "数据文件不能变");
    // 索引没了也不影响读数据
    assert!(xirang_core::tree::Store::load(&info.files[0]).is_ok());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn forget_removes_entries_but_keeps_data() {
    let dir = tmp_dir("forget");
    let info = build(&dir);
    let victim = info.files[0].to_string_lossy().into_owned();
    let n = wsidx::forget_files(&dir, &[victim.clone()]).unwrap();
    assert_eq!(n, 1);
    let listed = wsidx::files_status(&dir).unwrap();
    assert_eq!(listed.len(), info.files.len() - 1);
    assert!(!listed.iter().any(|f| f.path == victim));
    // 数据文件仍在、仍可读、仍合法
    assert!(Path::new(&victim).exists());
    let store = xirang_core::tree::Store::load(Path::new(&victim)).unwrap();
    assert!(!store.is_empty());
    // 夹具里的引用是跨文件的，单文件校验会报 R001（这是夹具的正常现象，不是 forget 造成的）
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn unlock_reports_pid_and_removes_lock() {
    let dir = tmp_dir("unlock");
    let _info = build(&dir);
    let idx = wsidx::index_dir(&dir);
    assert!(wsidx::unlock(&dir).unwrap().is_none(), "没有锁时应返回 None");

    let fake_pid = 999_999; // 几乎不可能存在
    std::fs::write(idx.join("lock"), fake_pid.to_string()).unwrap();
    let got = wsidx::unlock(&dir).unwrap().expect("应报出 pid");
    assert_eq!(got.0, fake_pid);
    assert!(!got.1, "该 pid 不该被当成活着的进程");
    assert!(!idx.join("lock").exists(), "锁文件应被删除");
    std::fs::remove_dir_all(&dir).ok();
}
