//! 自动整理：命令先返回结果，随后由后台进程把索引压实。

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use xirang_core::fixture;

fn tmp_root(tag: &str) -> PathBuf {
    let n = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let d = std::env::temp_dir().join(format!("xr-maint-{tag}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn xr(root: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_xr"));
    c.current_dir(root);
    c.env("XIRANG_CATALOG", root.join("catalog.idx"));
    c
}

fn build_workspace(root: &Path) -> Vec<PathBuf> {
    let spec = fixture::FixtureSpec {
        kind: fixture::FixtureKind::Multi,
        files: 2,
        entries_per_file: 30,
        inherit_ratio: 1,
        seed: 3,
        name: "maint".into(),
    };
    let info = fixture::build(root, &spec).unwrap();
    let out = xr(root).args(["index", "rebuild"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    info.files
}

fn log_bytes(root: &Path) -> u64 {
    root.join(".xirang-index").join("loc.log").metadata().map(|m| m.len()).unwrap_or(0)
}

#[test]
fn write_triggers_background_compaction() {
    let root = tmp_root("auto");
    let files = build_workspace(&root);
    let f = files[0].file_name().unwrap().to_string_lossy().into_owned();

    // 阈值调到极低 → 这次写入之后必然需要整理
    let out = xr(&root)
        .env("XIRANG_INDEX_COMPACT_RATIO", "0.001")
        .env("XIRANG_INDEX_COMPACT_MIN_BYTES", "0")
        .args(["new", &f, "nil", "新节点", "--no-history"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("后台开始整理"), "应提示已在后台整理：{err}");

    // 后台进程应当把日志压实掉（轮询等待，最多 10 秒）
    let before = log_bytes(&root);
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut shrunk = false;
    while Instant::now() < deadline {
        if log_bytes(&root) < before.max(4096) / 2 {
            shrunk = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(shrunk, "日志应被后台压实（前 {before} 字节，现 {} 字节）", log_bytes(&root));
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn maintenance_can_be_turned_off() {
    let root = tmp_root("off");
    let files = build_workspace(&root);
    let f = files[0].file_name().unwrap().to_string_lossy().into_owned();
    let out = xr(&root)
        .env("XIRANG_INDEX_MAINTENANCE", "off")
        .env("XIRANG_INDEX_COMPACT_RATIO", "0.001")
        .env("XIRANG_INDEX_COMPACT_MIN_BYTES", "0")
        .args(["new", &f, "nil", "新节点", "--no-history"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!err.contains("后台开始整理"), "关掉之后不该启动后台整理：{err}");
    std::fs::remove_dir_all(&root).ok();
}
