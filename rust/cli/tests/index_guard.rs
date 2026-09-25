//! 索引管理命令的护栏（常驻回归）：破坏性命令必须显式确认，JSON 可被消费。

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn tmp_root(tag: &str) -> PathBuf {
    let n = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let d = std::env::temp_dir().join(format!("xr-guard-{tag}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn xr(root: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_xr"));
    c.current_dir(root);
    c.env("XIRANG_CATALOG", root.join("catalog.idx"));
    c.env("XIRANG_INDEX_MAINTENANCE", "off");
    c
}

fn run(root: &Path, args: &[&str]) -> (i32, String, String) {
    let out = xr(root).args(args).output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// 用 CLI 本身造一个小工作区（不依赖任何夹具生成器）
fn workspace(root: &Path) {
    run(root, &["new", "a.xirang", "nil", "根", "--no-history"]);
    run(root, &["new", "b.xirang", "nil", "乙", "--no-history"]);
    let (code, _, err) = run(root, &["index", "rebuild"]);
    assert_eq!(code, 0, "{err}");
}

#[test]
fn drop_and_forget_need_yes() {
    let root = tmp_root("yes");
    workspace(&root);
    let idx = root.join(".xirang-index");
    assert!(idx.exists());

    let (code, _, err) = run(&root, &["index", "drop"]);
    assert_eq!(code, 2, "drop 无 --yes 应被拦下");
    assert!(err.contains("保护"), "{err}");
    assert!(idx.exists(), "被拦下时不能删");

    let (code, _, err) = run(&root, &["index", "forget", "a.xirang"]);
    assert_eq!(code, 2, "forget 无 --yes 应被拦下");
    assert!(err.contains("保护"), "{err}");

    let (code, out, _) = run(&root, &["index", "drop", "--yes", "--dry-run"]);
    assert_eq!(code, 0);
    assert!(out.contains("预演"), "{out}");
    assert!(idx.exists(), "预演不能删");

    let (code, _, _) = run(&root, &["index", "drop", "--yes"]);
    assert_eq!(code, 0);
    assert!(!idx.exists(), "带 --yes 才真的删");
    assert!(root.join("a.xirang").exists(), "数据文件不能动");
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn forget_removes_one_file_and_json_is_parseable() {
    let root = tmp_root("forget");
    workspace(&root);

    let (code, out, _) = run(&root, &["index", "status", "--json"]);
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(&out).expect("status --json 应是合法 JSON");
    assert_eq!(v["mode"], "workspace");
    assert_eq!(v["files"], 2);

    let (code, _, _) = run(&root, &["index", "forget", "a.xirang", "--yes"]);
    assert_eq!(code, 0);
    let (_, out, _) = run(&root, &["index", "files", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["files"].as_array().unwrap().len(), 1, "应只剩一个文件");
    assert!(root.join("a.xirang").exists(), "数据文件要留着");

    let (code, out, _) = run(&root, &["index", "path"]);
    assert_eq!(code, 0);
    assert!(Path::new(out.trim()).is_dir(), "path 应打印真实存在的目录");
    std::fs::remove_dir_all(&root).ok();
}
