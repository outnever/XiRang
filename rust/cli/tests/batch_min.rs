//! 批量提交（`xr batch`）的常驻护栏：一个进程改一批，
//! 预演不写、整批校验、任一条坏就整批不动，结果能被 `xr` 自己读回来。

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

fn tmp_root(tag: &str) -> PathBuf {
    let n = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let d = std::env::temp_dir().join(format!("xr-batch-{tag}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn run(root: &Path, args: &[&str], stdin: Option<&str>) -> (i32, String, String) {
    let mut c = Command::new(env!("CARGO_BIN_EXE_xr"));
    c.current_dir(root);
    c.env("XIRANG_CATALOG", root.join("catalog.idx"));
    c.env("XIRANG_INDEX_MAINTENANCE", "off");
    c.env("XIRANG_WORKSPACE", root);
    c.args(args);
    match stdin {
        Some(text) => {
            c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
            let mut child = c.spawn().unwrap();
            child.stdin.as_mut().unwrap().write_all(text.as_bytes()).unwrap();
            let out = child.wait_with_output().unwrap();
            (
                out.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&out.stdout).into_owned(),
                String::from_utf8_lossy(&out.stderr).into_owned(),
            )
        }
        None => {
            let out = c.output().unwrap();
            (
                out.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&out.stdout).into_owned(),
                String::from_utf8_lossy(&out.stderr).into_owned(),
            )
        }
    }
}

/// 建一个根 + n 个子节点，返回 子节点编号。
fn build(root: &Path, n: usize) -> Vec<String> {
    let (code, out, err) = run(root, &["new", "a.xirang", "nil", "根"], None);
    assert_eq!(code, 0, "{err}");
    let root_id = out
        .split('<')
        .nth(1)
        .and_then(|s| s.split('>').next())
        .expect("取根编号")
        .to_string();
    let mut ids = Vec::new();
    for i in 0..n {
        let (code, out, err) =
            run(root, &["new", "a.xirang", &root_id, &format!("词{i}"), "旧"], None);
        assert_eq!(code, 0, "{err}");
        ids.push(
            out.split('<')
                .nth(1)
                .and_then(|s| s.split('>').next())
                .expect("取子编号")
                .to_string(),
        );
    }
    ids
}

fn size(root: &Path) -> u64 {
    std::fs::metadata(root.join("a.xirang")).unwrap().len()
}

#[test]
fn batch_dry_run_then_apply_then_refuse_a_bad_line() {
    let root = tmp_root("apply");
    let ids = build(&root, 5);

    // 五条改动：前四条改值，第五条改名（同一批里）
    let mut list = String::new();
    for (i, id) in ids.iter().enumerate() {
        list.push_str(&format!("{{\"op\":\"set\",\"id\":\"{id}\",\"value\":\"新{i}\"}}\n"));
    }
    list.push_str(&format!(
        "{{\"op\":\"rename\",\"id\":\"{}\",\"name\":\"改过\"}}\n",
        ids[0]
    ));
    std::fs::write(root.join("list.jsonl"), &list).unwrap();

    // 预演：一个字节都不写
    let before = size(&root);
    let (code, out, err) = run(&root, &["batch", "a.xirang", "list.jsonl", "--dry-run"], None);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("预演"), "{out}");
    assert!(out.contains("会改到 5 个节点"), "{out}");
    assert_eq!(size(&root), before, "预演不写文件");

    // 真跑（不留痕）：一个进程改完
    let (code, out, err) =
        run(&root, &["batch", "a.xirang", "list.jsonl", "--no-history"], None);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("已批量提交") && out.contains("改到 5 个节点"), "{out}");
    let (code, out, err) = run(&root, &["find", "a.xirang", "新4"], None);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains(&ids[4]), "改完要读得到：{out}");
    let (code, out, err) = run(&root, &["tree", "a.xirang", "--no-pager"], None);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("改过 = 新0"), "改名要叠在改值之后：{out}");

    // 坏行：整批不动，退出码 2
    let bad = format!(
        "{{\"op\":\"set\",\"id\":\"{}\",\"value\":\"这行是好的\"}}\n{{\"op\":\"set\",\"id\":\"00000000-0000-0000-0000-000000000000\",\"value\":\"坏\"}}\n",
        ids[1]
    );
    std::fs::write(root.join("bad.jsonl"), bad).unwrap();
    let before = size(&root);
    let (code, _, err) = run(&root, &["batch", "a.xirang", "bad.jsonl", "--no-history"], None);
    assert_eq!(code, 2, "坏行必须整批拒绝");
    assert!(err.contains("第 2 条"), "报错要带条号：{err}");
    assert_eq!(size(&root), before, "拒绝时文件不许变");
    let (code, out, _) = run(&root, &["find", "a.xirang", "这行是好的"], None);
    assert_eq!(code, 0);
    assert!(out.contains("0 个匹配"), "前面那条也不该落地：{out}");

    // 从 stdin 读清单（`-`），并给个 JSON 结果
    let one = format!("{{\"op\":\"set\",\"id\":\"{}\",\"value\":\"走stdin\"}}\n", ids[2]);
    let (code, out, err) =
        run(&root, &["batch", "a.xirang", "-", "--no-history", "--json"], Some(&one));
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("\"changed\":1"), "{out}");

    // 校验与台账都没问题
    let (code, out, err) = run(&root, &["validate", "a.xirang", "--no-pager"], None);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("0 错误"), "{out}");
    let (code, out, err) = run(&root, &["index", "status"], None);
    assert_eq!(code, 0, "{err}");
    assert!(!out.contains("指纹不符"), "批量之后台账要新鲜：{out}");

    std::fs::remove_dir_all(&root).ok();
}

/// 复制到干净目录（文件还没登记）→ 批量照样能跑，不该把迁移卡在第一步。
#[test]
fn batch_on_an_unregistered_copy_works() {
    let root = tmp_root("unreg");
    let ids = build(&root, 3);
    // 复制到子目录：新副本天然没登记
    std::fs::create_dir_all(root.join("干净")).unwrap();
    std::fs::copy(root.join("a.xirang"), root.join("干净/副本.xirang")).unwrap();
    let list = format!("{{\"op\":\"set\",\"id\":\"{}\",\"value\":\"改过\"}}\n", ids[0]);
    std::fs::write(root.join("干净/l.jsonl"), &list).unwrap();

    let (code, out, err) = run(
        &root,
        &["batch", "干净/副本.xirang", "干净/l.jsonl", "--no-history"],
        None,
    );
    assert_eq!(code, 0, "没登记过也应当能跑：{err}");
    assert!(out.contains("已批量提交"), "{out}");
    let (_, out, _) = run(&root, &["find", "干净/副本.xirang", "改过"], None);
    assert!(out.contains(&ids[0]), "{out}");
    std::fs::remove_dir_all(&root).ok();
}

/// 词库分片：一次提交跨多个分片，内部按文件分组，输出逐文件报账。
#[test]
fn batch_over_a_shard_directory() {
    let root = tmp_root("shards");
    // 两棵根 → 拆成两个分片
    let (_, out, _) = run(&root, &["new", "词库.xirang", "nil", "甲根"], None);
    let a_root = grab(&out);
    let (_, out, _) = run(&root, &["new", "词库.xirang", "nil", "乙根"], None);
    let b_root = grab(&out);
    let (_, out, _) = run(&root, &["new", "词库.xirang", &a_root, "词1"], None);
    let a = grab(&out);
    let (_, out, _) = run(&root, &["new", "词库.xirang", &b_root, "词2"], None);
    let b = grab(&out);
    let (code, _, err) = run(
        &root,
        &["collection", "split", "词库.xirang", "--rule", "root", "--out", "分片"],
        None,
    );
    assert_eq!(code, 0, "{err}");

    let list = format!(
        "{{\"op\":\"rename\",\"id\":\"{a}\",\"name\":\"词1改\"}}\n{{\"op\":\"rename\",\"id\":\"{b}\",\"name\":\"词2改\"}}\n"
    );
    std::fs::write(root.join("跨.jsonl"), &list).unwrap();

    // 预演：两个分片都要校验过，且不写文件
    let (code, out, err) = run(&root, &["batch", "分片", "跨.jsonl", "--dry-run"], None);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("会改到 2 个节点"), "{out}");
    assert_eq!(out.matches(".xirang →").count(), 2, "逐分片报账：{out}");

    // 真跑
    let (code, out, err) = run(&root, &["batch", "分片", "跨.jsonl", "--no-history"], None);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("已批量提交") && out.contains("改到 2 个节点"), "{out}");
    assert_eq!(out.matches(".xirang →").count(), 2, "逐分片报账：{out}");

    // 两个分片各自都改到了
    let mut seen = 0;
    for entry in std::fs::read_dir(root.join("分片")).unwrap().flatten() {
        let p = entry.path();
        if p.extension().map(|e| e == "xirang").unwrap_or(false) {
            let name = p.file_name().unwrap().to_string_lossy().into_owned();
            let (_, out, _) = run(&root, &["find", &format!("分片/{name}"), "词1改"], None);
            if out.contains(&a) {
                seen += 1;
            }
            let (_, out, _) = run(&root, &["find", &format!("分片/{name}"), "词2改"], None);
            if out.contains(&b) {
                seen += 1;
            }
        }
    }
    assert_eq!(seen, 2, "两个分片各改到一条");
    std::fs::remove_dir_all(&root).ok();
}

fn grab(out: &str) -> String {
    out.split('<')
        .nth(1)
        .and_then(|s| s.split('>').next())
        .expect("取节点编号")
        .to_string()
}
