//! `xr history prune` / MCP `node(prune_history)`：裁剪留痕的护栏与效果。

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

fn tmp_root(tag: &str) -> PathBuf {
    let n = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let d = std::env::temp_dir().join(format!("xr-prune-{tag}-{}-{n}", std::process::id()));
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

/// 造一个改过 20 次的节点，返回 (文件名, 节点 id)
fn fixture(root: &Path) -> (String, String) {
    let (_, out, _) = run(root, &["new", "a.xirang", "nil", "根"]);
    let id = out
        .split('<')
        .nth(1)
        .and_then(|s| s.split('>').next())
        .expect("取节点编号")
        .to_string();
    for i in 0..20 {
        run(root, &["set", "a.xirang", &id, &format!("值{i}")]);
    }
    ("a.xirang".to_string(), id)
}

fn snapshots(root: &Path, file: &str, id: &str) -> usize {
    let (_, out, _) = run(root, &["history", file, id]);
    out.lines().filter(|l| l.trim_start().starts_with("快照：")).count()
}

#[test]
fn prune_needs_yes_and_dry_run_does_not_touch_the_file() {
    let root = tmp_root("guard");
    let (file, id) = fixture(&root);
    let before = std::fs::metadata(root.join(&file)).unwrap().len();
    assert_eq!(snapshots(&root, &file, &id), 20);

    let (code, _, err) = run(&root, &["history", "prune", &file, &id, "--keep", "5"]);
    assert_eq!(code, 2, "没有 --yes 应被拦下");
    assert!(err.contains("回滚能力"), "{err}");
    assert_eq!(snapshots(&root, &file, &id), 20, "被拦下时不能裁");

    let (code, out, _) = run(&root, &["history", "prune", &file, &id, "--keep", "5", "--dry-run", "--yes"]);
    assert_eq!(code, 0);
    assert!(out.contains("预演"), "{out}");
    assert_eq!(std::fs::metadata(root.join(&file)).unwrap().len(), before, "预演不能改文件");
    assert_eq!(snapshots(&root, &file, &id), 20);
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn prune_keeps_the_newest_and_shrinks_the_file() {
    let root = tmp_root("prune");
    let (file, id) = fixture(&root);
    let before = std::fs::metadata(root.join(&file)).unwrap().len();

    let (code, out, _) = run(&root, &["history", "prune", &file, &id, "--keep", "5", "--yes"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("已裁剪 15 条"), "{out}");
    assert_eq!(snapshots(&root, &file, &id), 5, "只留最近 5 条");
    let after = std::fs::metadata(root.join(&file)).unwrap().len();
    assert!(after < before, "文件应变小：{before} → {after}");

    // 保留的最近一条仍可回滚
    let (code, out, _) = run(&root, &["revert", &file, &id]);
    assert_eq!(code, 0, "{out}");
    let (_, out, _) = run(&root, &["cat", &file]);
    // 快照存的是「改之前的值」：20 次修改留下 [空, 值0 … 值18]，
    // 裁到最近 5 条后回滚应落到 值18（cat 的第一行就是节点本身）
    assert!(
        out.lines().next().unwrap_or("").contains("根 = 值18"),
        "应回滚到最近一条保留下来的快照：{out}"
    );
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn mcp_tool_can_prune_history_with_force() {
    let root = tmp_root("mcp");
    let (file, id) = fixture(&root);
    let mut child = Command::new(env!("CARGO_BIN_EXE_xr-mcp"))
        .arg("--root")
        .arg(&root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let call = |stdin: &mut std::process::ChildStdin,
                stdout: &mut BufReader<std::process::ChildStdout>,
                args: serde_json::Value| {
        writeln!(
            stdin,
            "{}",
            serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
                "params":{"name":"node","arguments":args}})
        )
        .unwrap();
        stdin.flush().unwrap();
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        v["result"].clone()
    };

    // 不带 force → guarded
    let res = call(
        &mut stdin,
        &mut stdout,
        serde_json::json!({"file": file, "action": "prune_history", "node": id, "keep": 3}),
    );
    assert_eq!(res["isError"], true, "{res}");
    assert!(res["content"][0]["text"].as_str().unwrap().contains("guarded"), "{res}");

    // 带 force → 成功并缩小
    let res = call(
        &mut stdin,
        &mut stdout,
        serde_json::json!({"file": file, "action": "prune_history", "node": id, "keep": 3, "force": true}),
    );
    assert_eq!(res["isError"].as_bool().unwrap_or(false), false, "{res}");
    let body: serde_json::Value = serde_json::from_str(res["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(body["removed"], 17);
    assert_eq!(body["kept"], 3);
    let _ = child.kill();
    let _ = child.wait();
    assert_eq!(snapshots(&root, &file, &id), 3);
    std::fs::remove_dir_all(&root).ok();
}
