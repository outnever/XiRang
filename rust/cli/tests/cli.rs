//! xr CLI 集成测试：进程级运行 `xr` 二进制，验证 info / tree / validate / 编辑 / 导出往返。

use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use xirang_core::codec::{Node, Uuid, Value};
use xirang_core::shard;
use xirang_core::tree::Store;

fn xr() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_xr"));
    // 隔离本机目录，避免测试污染真实 ~/.config
    let cat = std::env::temp_dir().join(format!("xr-test-catalog-{}.idx", std::process::id()));
    c.env("XIRANG_CATALOG", cat);
    c
}

fn tmp_xirang(tag: &str) -> std::path::PathBuf {
    let n = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    std::env::temp_dir().join(format!("xr-cli-{tag}-{}-{n}.xirang", std::process::id()))
}

#[test]
fn info_validate_tree() {
    let path = tmp_xirang("basic");
    let mut store = Store::new();
    let root = store.create(None, "根", Value::Empty, false);
    store.create(Some(root.id), "子", Value::Text("hello".into()), false);
    store.save(&path).unwrap();

    // info
    let out = xr().args(["info", path.to_str().unwrap()]).output().unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("节点数: 2"), "info 输出：{stdout}");

    // validate
    let out = xr().args(["validate", path.to_str().unwrap()]).output().unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8(out.stdout).unwrap().contains("0 错误"));

    // tree
    let out = xr().args(["tree", path.to_str().unwrap()]).output().unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("根"), "tree 输出：{stdout}");
    assert!(stdout.contains("子 = hello"));

    std::fs::remove_file(&path).ok();
}

#[test]
fn export_import_roundtrip() {
    let path = tmp_xirang("rt");
    let mut store = Store::new();
    let root = store.create(None, "根", Value::Empty, false);
    store.create(Some(root.id), "甲", Value::Int(42), false);
    store.save(&path).unwrap();

    // 导出 JSON
    let out = xr().args(["export", path.to_str().unwrap(), "json"]).output().unwrap();
    assert!(out.status.success());
    let json = String::from_utf8(out.stdout).unwrap();
    let json_path = tmp_xirang("rt.json");
    std::fs::write(&json_path, &json).unwrap();

    // 导入回新文件
    let out2 = xr().args(["import", tmp_xirang("rt2").to_str().unwrap(), "json", json_path.to_str().unwrap()]).output().unwrap();
    assert!(out2.status.success());

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&json_path).ok();
    std::fs::remove_file(tmp_xirang("rt2")).ok();
}

#[test]
fn rename_keeps_id_value_and_children() {
    let path = tmp_xirang("rename");
    let mut store = Store::new();
    let root = store.create(None, "根", Value::Empty, false);
    let child = store.create(Some(root.id), "旧名", Value::Text("值".into()), false);
    store.create(Some(child.id), "子", Value::Empty, false);
    store.save(&path).unwrap();

    let out = xr()
        .args(["rename", path.to_str().unwrap(), &child.id.to_string(), "新名"])
        .output()
        .unwrap();
    assert!(out.status.success(), "stderr：{}", String::from_utf8_lossy(&out.stderr));

    let after = Store::load(&path).unwrap();
    let c = after.get(child.id).unwrap();
    assert_eq!(c.name, "新名");
    assert_eq!(c.value, Value::Text("值".into()));   // 值不变
    assert!(after.child_by_name(c, "子").is_some());  // 子节点还在
    assert!(after.child_by_name(c, "@history").is_some()); // 旧名字进了 @history
    assert!(after.get(root.id).is_some());            // 编号没变

    let out = xr().args(["validate", path.to_str().unwrap()]).output().unwrap();
    assert!(String::from_utf8(out.stdout).unwrap().contains("0 错误"));

    // 空名字：拒绝
    let out = xr()
        .args(["rename", path.to_str().unwrap(), &child.id.to_string(), ""])
        .output()
        .unwrap();
    assert!(!out.status.success());

    std::fs::remove_file(&path).ok();
}

#[test]
fn tree_head_limits_printed_nodes() {
    let path = tmp_xirang("head");
    let mut store = Store::new();
    let root = store.create(None, "根", Value::Empty, false);
    for i in 0..5 {
        store.create(Some(root.id), &format!("子{i}"), Value::Empty, false);
    }
    store.save(&path).unwrap();

    // 不带 --head：6 个节点全打
    let out = xr().args(["tree", path.to_str().unwrap()]).output().unwrap();
    let full = String::from_utf8(out.stdout).unwrap();
    assert_eq!(full.lines().filter(|l| !l.trim().is_empty()).count(), 6);

    // --head 3：只打 3 个；提示走 stderr，stdout 保持干净（可直接管道）
    let out = xr()
        .args(["tree", path.to_str().unwrap(), "--head", "3"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(stdout.lines().filter(|l| !l.trim().is_empty()).count(), 3);
    assert!(!stdout.contains("只打印了"), "提示不该混进 stdout：{stdout}");
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("只打印了前 3 个节点，共 6 个"), "stderr：{stderr}");

    std::fs::remove_file(&path).ok();
}

/// 造一棵 3 层的小树：根 → 甲/乙，且甲下面还有一个丙。
fn three_level_store() -> Store {
    let mut store = Store::new();
    let root = store.create(None, "根", Value::Empty, false);
    let a = store.create(Some(root.id), "甲", Value::Empty, false);
    store.create(Some(a.id), "丙", Value::Empty, false);
    store.create(Some(root.id), "乙", Value::Empty, false);
    store
}

#[test]
fn tree_depth_limits_expansion() {
    let path = tmp_xirang("depth");
    three_level_store().save(&path).unwrap();

    let out = xr().args(["tree", path.to_str().unwrap()]).output().unwrap();
    let full = String::from_utf8(out.stdout).unwrap();
    assert_eq!(full.lines().filter(|l| !l.trim().is_empty()).count(), 4);

    // --depth 1：只有根
    let out = xr()
        .args(["tree", path.to_str().unwrap(), "--depth", "1"])
        .output()
        .unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(stdout.lines().filter(|l| !l.trim().is_empty()).count(), 1);
    assert!(stdout.contains("根"));
    assert!(String::from_utf8(out.stderr).unwrap().contains("只展开到第 1 层"));

    // --depth 2：根 + 甲 + 乙（丙被截掉）
    let out = xr()
        .args(["tree", path.to_str().unwrap(), "--depth", "2"])
        .output()
        .unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(stdout.lines().filter(|l| !l.trim().is_empty()).count(), 3);
    assert!(!stdout.contains("丙"));

    std::fs::remove_file(&path).ok();
}

#[test]
fn cat_is_flat_and_honours_head_and_ids() {
    let path = tmp_xirang("cat");
    three_level_store().save(&path).unwrap();

    // 扁平：一行一个节点、按存放顺序、没有树形连接符
    let out = xr().args(["cat", path.to_str().unwrap()]).output().unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(lines, vec!["根", "甲", "丙", "乙"], "应按存放顺序：{stdout}");
    assert!(!stdout.contains("├─"), "cat 不该有树形连接符：{stdout}");

    // --head：限制条数，提示走 stderr
    let out = xr()
        .args(["cat", path.to_str().unwrap(), "--head", "2"])
        .output()
        .unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(stdout.lines().filter(|l| !l.trim().is_empty()).count(), 2);
    assert!(String::from_utf8(out.stderr).unwrap().contains("只打印了前 2 个节点"));

    // --ids：每个节点后面带编号
    let out = xr()
        .args(["cat", path.to_str().unwrap(), "--ids"])
        .output()
        .unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(stdout.matches('<').count(), 4, "每个节点后面都该有编号：{stdout}");

    std::fs::remove_file(&path).ok();
}

#[test]
fn bad_magic_reports_f001() {
    let path = tmp_xirang("bad");
    std::fs::write(&path, b"not-a-xirang-file").unwrap();
    let out = xr().args(["info", path.to_str().unwrap()]).output().unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("F001"), "stderr：{stderr}");
    std::fs::remove_file(&path).ok();
}

#[test]
fn ws_and_index_lazy_cross_file() {
    let a = tmp_xirang("ws-a");
    let b = tmp_xirang("ws-b");

    let mut sb = Store::new();
    let b_root = sb.create(None, "乙", Value::Empty, false);
    let child = sb.create(Some(b_root.id), "词义", Value::Text("目标".into()), false);
    sb.save(&b).unwrap();

    let mut sa = Store::new();
    let a_root = sa.create(None, "甲", Value::Empty, false);
    sa.create(Some(a_root.id), "指向", Value::Reference(child.id), false);
    sa.save(&a).unwrap();

    // 两种索引模式都要能跨文件解析（默认工作区台账 / XIRANG_INDEX_MODE=sidecar）
    for mode in ["workspace", "sidecar"] {
        let mut c = xr();
        c.env("XIRANG_INDEX_MODE", mode);
        // 工作区台账模式需要先建索引；侧车模式按需自建
        if mode == "workspace" {
            let out = c
                .args(["index", "rebuild", a.to_str().unwrap(), b.to_str().unwrap()])
                .output()
                .unwrap();
            assert!(out.status.success(), "rebuild 输出：{}", String::from_utf8_lossy(&out.stderr));
        }
        let mut c = xr();
        c.env("XIRANG_INDEX_MODE", mode);
        let out = c
            .args(["ws", &child.id.to_string(), a.to_str().unwrap(), b.to_str().unwrap()])
            .output()
            .unwrap();
        assert!(out.status.success());
        let stdout = String::from_utf8(out.stdout).unwrap();
        assert!(stdout.contains("词义") && stdout.contains("指向"), "{mode} 模式 ws 输出：{stdout}");
    }

    // 工作区台账模式的索引命令
    let out = xr().args(["index", "status", a.to_str().unwrap()]).output().unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("索引模式: workspace"), "status 输出：{stdout}");
    let out = xr().args(["index", "compact", a.to_str().unwrap()]).output().unwrap();
    assert!(out.status.success());
    let out = xr().args(["index", "check", a.to_str().unwrap()]).output().unwrap();
    assert!(out.status.success());
    let out = xr().args(["index", "gc", a.to_str().unwrap()]).output().unwrap();
    assert!(out.status.success());

    for p in [&a, &b] {
        std::fs::remove_file(p).ok();
        let mut sp = p.as_os_str().to_os_string();
        sp.push(".idx");
        std::fs::remove_file(std::path::PathBuf::from(sp)).ok();
    }
    // 清掉工作区索引目录
    if let Some(dir) = a.parent() {
        std::fs::remove_dir_all(dir.join(".xirang-index")).ok();
    }
}

#[test]
fn collection_split_list_compact() {
    let file = tmp_xirang("coll");
    let mut s = Store::new();
    let root = s.create(None, "词库", Value::Empty, false);
    let e1 = s.create(Some(root.id), "词条", Value::Empty, false);
    s.create(Some(e1.id), "词形", Value::Text("灯".into()), false);
    let e2 = s.create(Some(root.id), "词条", Value::Empty, false);
    s.create(Some(e2.id), "词形", Value::Text("火".into()), false);
    s.save(&file).unwrap();

    let dir = std::env::temp_dir().join(format!(
        "xr-cli-coll-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
    ));

    let out = xr()
        .args(["collection", "split", file.to_str().unwrap(), "--rule", "name:词条", "--out", dir.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("3 个分片"), "split 输出：{stdout}");

    let out = xr().args(["collection", "list", dir.to_str().unwrap()]).output().unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("判定器"), "list 输出：{stdout}");

    let out = xr().args(["compact", dir.to_str().unwrap(), "--all"]).output().unwrap();
    assert!(out.status.success());

    std::fs::remove_file(&file).ok();
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn collection_write_commands_target_shard() {
    let file = tmp_xirang("collw");
    let mut s = Store::new();
    let root = s.create(None, "词库", Value::Empty, false);
    let e1 = s.create(Some(root.id), "词条", Value::Empty, false);
    let xing = s.create(Some(e1.id), "词形", Value::Text("灯".into()), false);
    s.save(&file).unwrap();

    let dir = std::env::temp_dir().join(format!(
        "xr-cli-collw-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
    ));
    let split = xr()
        .args(["collection", "split", file.to_str().unwrap(), "--rule", "name:词条", "--out", dir.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(split.status.success());

    // set：只改 词形 所在分片
    let out = xr().args(["set", dir.to_str().unwrap(), &xing.id.to_string(), "火"]).output().unwrap();
    assert!(out.status.success());
    let shard_path = dir.join(format!("{}.xirang", e1.id));
    let folded = shard::fold(&Store::load(&shard_path).unwrap());
    assert_eq!(folded.get(xing.id).unwrap().value, Value::Text("火".into()));

    // new nil：新建一个分片
    let out = xr().args(["new", dir.to_str().unwrap(), "nil", "新词条"]).output().unwrap();
    assert!(out.status.success());

    // rm：置空
    let out = xr().args(["rm", dir.to_str().unwrap(), &xing.id.to_string()]).output().unwrap();
    assert!(out.status.success());
    let folded = shard::fold(&Store::load(&shard_path).unwrap());
    assert_eq!(folded.get(xing.id).unwrap().value, Value::Empty);

    // 清单现在应含 3 个分片（词库 + 词条 + 新词条）
    let list = xr().args(["collection", "list", dir.to_str().unwrap()]).output().unwrap();
    assert!(list.status.success());
    let stdout = String::from_utf8(list.stdout).unwrap();
    assert!(stdout.contains("3 个"), "list 输出：{stdout}");

    std::fs::remove_file(&file).ok();
    std::fs::remove_dir_all(&dir).ok();
}

fn xr_cat(cat: &std::path::Path) -> Command {
    let mut c = xr();
    c.env("XIRANG_CATALOG", cat);
    c
}

#[test]
fn catalog_same_uuid_is_normal_union_and_only() {
    let dir = std::env::temp_dir().join(format!(
        "xr-cli-cat-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let cat = dir.join("catalog.idx");
    let f1 = dir.join("a.xirang");
    let f2 = dir.join("b.xirang");
    let shared = Uuid::random_v4();
    let node = |id: Uuid, parent: Uuid, name: &str, v: &str| Node {
        id,
        parent: Some(parent),
        name: name.to_string(),
        value: Value::Text(v.to_string()),
    };

    let mut s1 = Store::new();
    let r1 = s1.create(None, "甲", Value::Empty, false);
    s1.add(node(shared, r1.id, "共享", "同"));
    s1.create(Some(shared), "形态", Value::Text("甲库的孩子".into()), false);
    s1.save(&f1).unwrap();

    let mut s2 = Store::new();
    let r2 = s2.create(None, "乙", Value::Empty, false);
    s2.add(node(shared, r2.id, "共享", "同"));
    s2.create(Some(shared), "形态", Value::Text("乙库的孩子".into()), false);
    s2.save(&f2).unwrap();

    // 读命令自动登记
    let out = xr_cat(&cat).args(["info", f1.to_str().unwrap()]).output().unwrap();
    assert!(out.status.success());
    let list = xr_cat(&cat).args(["catalog", "list"]).output().unwrap();
    let stdout = String::from_utf8(list.stdout).unwrap();
    assert!(stdout.contains("a.xirang"), "list: {stdout}");

    // 显式扫描两个文件
    let out = xr_cat(&cat)
        .args(["catalog", "scan", dir.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(out.status.success());

    // 同编号多文件不是冲突：自身名字/值一致 → check 不报
    let out = xr_cat(&cat).args(["catalog", "check"]).output().unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(!stdout.contains(&shared.to_string()), "check 不应报同编号: {stdout}");

    // ws 默认跨文件并集：两份都在、孩子取并集、每条标来源
    let out = xr_cat(&cat)
        .args(["ws", &shared.to_string(), f1.to_str().unwrap(), f2.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("2 处"), "ws 两份: {stdout}");
    assert!(stdout.contains("甲库的孩子"), "ws 并集含甲孩子: {stdout}");
    assert!(stdout.contains("乙库的孩子"), "ws 并集含乙孩子: {stdout}");
    assert!(
        stdout.contains("a.xirang") && stdout.contains("b.xirang"),
        "ws 标来源: {stdout}"
    );

    // --only：只看一个文件的那份（孩子也只来自该文件）
    let out = xr_cat(&cat)
        .args([
            "ws",
            &shared.to_string(),
            f1.to_str().unwrap(),
            "--only",
            f1.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("甲库的孩子"), "only 含本文件孩子: {stdout}");
    assert!(!stdout.contains("乙库的孩子"), "only 不应含别文件孩子: {stdout}");

    // --no-index 不写入（扫描后目录里 b 仍在，但 info 不带 --no-index 也不会新增别的）
    let before = std::fs::read(&cat).unwrap();
    let out = xr_cat(&cat)
        .args(["info", f2.to_str().unwrap(), "--no-index"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let after = std::fs::read(&cat).unwrap();
    assert_eq!(before, after, "--no-index 不应改动目录");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn ws_falls_back_to_catalog() {
    let dir = std::env::temp_dir().join(format!(
        "xr-cli-catws-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let cat = dir.join("catalog.idx");
    let f1 = dir.join("a.xirang");
    let f2 = dir.join("b.xirang");
    let target = Uuid::random_v4();

    // b.xirang：乙 + 目标节点（UUID = target）
    let mut s2 = Store::new();
    let r2 = s2.create(None, "乙", Value::Empty, false);
    s2.add(Node { id: target, parent: Some(r2.id), name: "词义".into(), value: Value::Text("目标".into()) });
    s2.save(&f2).unwrap();

    // a.xirang：甲 + 指向 target 的引用
    let mut s1 = Store::new();
    let r1 = s1.create(None, "甲", Value::Empty, false);
    s1.add(Node { id: Uuid::random_v4(), parent: Some(r1.id), name: "指向".into(), value: Value::Reference(target) });
    s1.save(&f1).unwrap();

    // 只扫进目录，不把 b.xirang 传给 ws
    let out = xr_cat(&cat).args(["catalog", "scan", dir.to_str().unwrap()]).output().unwrap();
    assert!(out.status.success());

    let out = xr_cat(&cat)
        .args(["ws", &target.to_string(), f1.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("b.xirang"), "ws 应经目录补进 b.xirang：{stdout}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn catalog_check_self_diff_and_sync_keeps_children() {
    let dir = std::env::temp_dir().join(format!(
        "xr-cli-catcheck-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let cat = dir.join("catalog.idx");
    let f1 = dir.join("a.xirang");
    let f2 = dir.join("b.xirang");
    let diff = Uuid::random_v4(); // 自身不同 → 应被列出
    let same = Uuid::random_v4(); // 自身相同、孩子不同 → 不应被列出

    let mut s1 = Store::new();
    let r1 = s1.create(None, "甲", Value::Empty, false);
    s1.add(Node { id: diff, parent: Some(r1.id), name: "甲称".into(), value: Value::Text("x".into()) });
    s1.create(Some(diff), "甲孩子", Value::Text("A".into()), false);
    s1.add(Node { id: same, parent: Some(r1.id), name: "同称".into(), value: Value::Text("s".into()) });
    s1.create(Some(same), "甲孩子", Value::Text("A".into()), false);
    s1.save(&f1).unwrap();

    let mut s2 = Store::new();
    let r2 = s2.create(None, "乙", Value::Empty, false);
    s2.add(Node { id: diff, parent: Some(r2.id), name: "乙称".into(), value: Value::Text("y".into()) });
    let keep = s2.create(Some(diff), "乙孩子", Value::Text("B".into()), false);
    s2.add(Node { id: same, parent: Some(r2.id), name: "同称".into(), value: Value::Text("s".into()) });
    s2.create(Some(same), "乙孩子", Value::Text("B".into()), false);
    s2.save(&f2).unwrap();

    let out = xr_cat(&cat).args(["catalog", "scan", dir.to_str().unwrap()]).output().unwrap();
    assert!(out.status.success());

    let out = xr_cat(&cat).args(["catalog", "check"]).output().unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains(&diff.to_string()), "check 应列出自身不同: {stdout}");
    assert!(stdout.contains("甲孩子") && stdout.contains("乙孩子"), "check 附两边孩子: {stdout}");
    assert!(!stdout.contains(&same.to_string()), "check 不应列出孩子不同: {stdout}");

    // 同步：只改自身名字/值，孩子不动
    let out = xr_cat(&cat)
        .args(["catalog", "check", "--sync", &diff.to_string(), "--base", f1.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(out.status.success(), "sync stderr: {}", String::from_utf8_lossy(&out.stderr));

    let after = Store::load(&f2).unwrap();
    let d = after.get(diff).unwrap();
    assert_eq!(d.name, "甲称");
    assert_eq!(d.value, Value::Text("x".into()));
    assert!(after.get(keep.id).is_some(), "乙孩子应保持原样");

    // 再 check 不再列出
    let out = xr_cat(&cat).args(["catalog", "check"]).output().unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(!stdout.contains(&diff.to_string()), "同步后不应再列出: {stdout}");

    std::fs::remove_dir_all(&dir).ok();
}
