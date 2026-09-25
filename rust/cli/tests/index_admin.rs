//! 索引管理命令的 CLI 行为：护栏、JSON、path、drop 的真实效果。

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use xirang_core::fixture;

fn tmp_root(tag: &str) -> PathBuf {
    let n = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let d = std::env::temp_dir().join(format!("xr-idxadm-{tag}-{}-{n}", std::process::id()));
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

fn build(root: &Path) -> fixture::FixtureInfo {
    let spec = fixture::FixtureSpec {
        kind: fixture::FixtureKind::Multi,
        files: 2,
        entries_per_file: 10,
        inherit_ratio: 1,
        seed: 4,
        name: "idxadm".into(),
    };
    let info = fixture::build(root, &spec).unwrap();
    let out = xr(root).args(["index", "rebuild"]).output().unwrap();
    assert!(out.status.success());
    info
}

fn run(root: &Path, args: &[&str]) -> (i32, String, String) {
    let out = xr(root).args(args).output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn destructive_commands_need_yes() {
    let root = tmp_root("guard");
    let _info = build(&root);
    let idx = root.join(".xirang-index");
    assert!(idx.exists());

    let (code, _, err) = run(&root, &["index", "drop"]);
    assert_eq!(code, 2, "drop 无 --yes 应被拦下");
    assert!(err.contains("保护"), "{err}");
    assert!(idx.exists(), "被拦下时不能删");

    let f = "idxadm-000.xirang";
    let (code, _, err) = run(&root, &["index", "forget", f]);
    assert_eq!(code, 2, "forget 无 --yes 应被拦下");
    assert!(err.contains("保护"), "{err}");

    // dry-run 也不动手
    let (code, out, _) = run(&root, &["index", "drop", "--yes", "--dry-run"]);
    assert_eq!(code, 0);
    assert!(out.contains("预演"), "{out}");
    assert!(idx.exists(), "dry-run 不能删");

    let (code, _, _) = run(&root, &["index", "drop", "--yes"]);
    assert_eq!(code, 0);
    assert!(!idx.exists(), "带 --yes 才真的删");
    assert!(root.join(f).exists(), "数据文件不能动");
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn json_output_and_path_are_usable() {
    let root = tmp_root("json");
    let _info = build(&root);

    let (code, out, _) = run(&root, &["index", "status", "--json"]);
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(&out).expect("status --json 应是合法 JSON");
    assert_eq!(v["mode"], "workspace");
    assert!(v["files"].as_u64().unwrap() >= 2);
    assert!(v["ledgers"]["loc"]["entries"].as_u64().unwrap() > 0);

    let (code, out, _) = run(&root, &["index", "files", "--json"]);
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(&out).expect("files --json 应是合法 JSON");
    assert!(v["files"].as_array().unwrap().len() >= 2);
    assert!(v["files"][0]["fresh"].as_bool().unwrap());

    let (code, out, _) = run(&root, &["index", "path"]);
    assert_eq!(code, 0);
    assert!(Path::new(out.trim()).is_dir(), "path 应打印真实存在的目录：{out}");
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn forget_removes_one_file_from_the_ledger() {
    let root = tmp_root("forget");
    let _info = build(&root);
    let f = "idxadm-000.xirang";
    let (code, _, _) = run(&root, &["index", "forget", f, "--yes"]);
    assert_eq!(code, 0);
    let (_, out, _) = run(&root, &["index", "files", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["files"].as_array().unwrap().len(), 1, "应只剩一个文件");
    assert!(root.join(f).exists(), "数据文件要留着");
    std::fs::remove_dir_all(&root).ok();
}
