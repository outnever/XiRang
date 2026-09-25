//! 留痕裁剪的常驻护栏：默认拦下、预演不动文件、确认后真正变小。

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn tmp_root(tag: &str) -> PathBuf {
    let n = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let d = std::env::temp_dir().join(format!("xr-prune-{tag}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn run(root: &Path, args: &[&str]) -> (i32, String, String) {
    let mut c = Command::new(env!("CARGO_BIN_EXE_xr"));
    c.current_dir(root);
    c.env("XIRANG_CATALOG", root.join("catalog.idx"));
    c.env("XIRANG_INDEX_MAINTENANCE", "off");
    let out = c.args(args).output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// 改同一个节点 20 次，返回节点编号
fn edits(root: &Path, n: usize) -> String {
    let (_, out, _) = run(root, &["new", "a.xirang", "nil", "根"]);
    let id = out
        .split('<')
        .nth(1)
        .and_then(|s| s.split('>').next())
        .expect("取节点编号")
        .to_string();
    for i in 0..n {
        run(root, &["set", "a.xirang", &id, &format!("值{i}")]);
    }
    id
}

fn snapshots(root: &Path, id: &str) -> usize {
    let (_, out, _) = run(root, &["history", "a.xirang", id]);
    out.lines().filter(|l| l.trim_start().starts_with("快照：")).count()
}

#[test]
fn prune_guard_dry_run_and_effect() {
    let root = tmp_root("effect");
    let id = edits(&root, 20);
    let before = std::fs::metadata(root.join("a.xirang")).unwrap().len();
    assert_eq!(snapshots(&root, &id), 20);

    // 默认拦下：丢掉的是回滚能力
    let (code, _, err) = run(&root, &["history", "prune", "a.xirang", &id, "--keep", "5"]);
    assert_eq!(code, 2);
    assert!(err.contains("回滚能力"), "{err}");
    assert_eq!(snapshots(&root, &id), 20, "被拦下时不能裁");

    // 预演：不动文件
    let (code, out, _) = run(
        &root,
        &["history", "prune", "a.xirang", &id, "--keep", "5", "--dry-run", "--yes"],
    );
    assert_eq!(code, 0);
    assert!(out.contains("预演"), "{out}");
    assert_eq!(std::fs::metadata(root.join("a.xirang")).unwrap().len(), before);

    // 确认后：只留最近 5 条，文件变小
    let (code, out, _) = run(&root, &["history", "prune", "a.xirang", &id, "--keep", "5", "--yes"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("已裁剪 15 条"), "{out}");
    assert_eq!(snapshots(&root, &id), 5);
    let after = std::fs::metadata(root.join("a.xirang")).unwrap().len();
    assert!(after < before, "文件应变小：{before} → {after}");

    // 保留的快照仍可回滚（快照存的是「改之前的值」，最近一条是 值18）
    let (code, _, _) = run(&root, &["revert", "a.xirang", &id]);
    assert_eq!(code, 0);
    let (_, out, _) = run(&root, &["cat", "a.xirang"]);
    assert!(
        out.lines().next().unwrap_or("").contains("根 = 值18"),
        "应回滚到保留下来的最近一条快照：{out}"
    );
    std::fs::remove_dir_all(&root).ok();
}
