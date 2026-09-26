//! 只追加写的常驻护栏：改一个词只往文件末尾补几条记录，旧的记录一个字节都不动。
//!
//! 这条护栏挡的是「悄悄退回整份重写」——整份重写没有错，但它会把 append-v1
//! 攒下来的历史顺手折掉（文件突然变小），而且大文件上要 4 秒。

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn tmp_root(tag: &str) -> PathBuf {
    let n = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let d = std::env::temp_dir().join(format!("xr-append-{tag}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn run(root: &Path, args: &[&str]) -> (i32, String, String) {
    let mut c = Command::new(env!("CARGO_BIN_EXE_xr"));
    c.current_dir(root);
    c.env("XIRANG_CATALOG", root.join("catalog.idx"));
    c.env("XIRANG_INDEX_MAINTENANCE", "off");
    c.env("XIRANG_WORKSPACE", root);
    let out = c.args(args).output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn size(root: &Path) -> u64 {
    std::fs::metadata(root.join("a.xirang")).unwrap().len()
}

#[test]
fn set_appends_instead_of_rewriting() {
    let root = tmp_root("append");
    let (code, out, _) = run(&root, &["new", "a.xirang", "nil", "根"]);
    assert_eq!(code, 0, "{out}");
    let id = out
        .split('<')
        .nth(1)
        .and_then(|s| s.split('>').next())
        .expect("取节点编号")
        .to_string();

    let before = size(&root);
    let (code, _, err) = run(&root, &["set", "a.xirang", &id, "第一个值"]);
    assert_eq!(code, 0, "{err}");
    let after = size(&root);

    // 只追加：长出来的必须远小于整份重写（整份是「原有全部记录 + 新记录」，
    // 这里只补「改动的那一条 + 留痕的两条 + 协议声明」）
    assert!(after > before, "文件应该变大：{before} → {after}");
    assert!(
        after - before < 600,
        "只改一个词不该长这么多：{before} → {after}（是不是退回整份重写了？）"
    );

    // 值读得回来，而且校验通过（重复编号靠 `@protocol = append-v1` 声明，不报 E002）
    let (code, out, err) = run(&root, &["find", "a.xirang", "第一个值"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains(&id), "应该找得到刚写进去的值：{out}");
    let (code, out, err) = run(&root, &["validate", "a.xirang", "--no-pager"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("0 错误"), "{out}");

    // 再改几次：协议声明是幂等的（不能每次编辑都多一个节点）
    for i in 0..3 {
        run(&root, &["set", "a.xirang", &id, &format!("第{i}次")]);
    }
    let (_, out, _) = run(&root, &["tree", "a.xirang", "--no-pager"]);
    assert_eq!(
        out.matches("@protocol = append-v1").count(),
        1,
        "`@protocol` 声明必须只有一条：{out}"
    );

    // 台账要跟得上（只追加登记），且点查结果来自台账而不是整份载入
    let (code, out, err) = run(&root, &["index", "check", "--sample", "5"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("一致"), "{out}");

    // 折叠（压实）：先预演（不动文件），再真折（变小），读出来的还是最后那个值
    let folded_before = size(&root);
    let (code, out, err) = run(&root, &["compact", "a.xirang", "--dry-run"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("预演"), "预演要说明自己在预演：{out}");
    assert!(out.contains("不跨文件"), "输出要写明折叠口径：{out}");
    assert!(out.contains("字节:"), "输出要报「字节多少 → 多少」：{out}");
    assert_eq!(size(&root), folded_before, "预演不能动文件");

    let (code, out, err) = run(&root, &["compact", "a.xirang"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("已折叠"), "{out}");
    assert!(out.contains("记录:") && out.contains("字节:"), "{out}");
    assert!(size(&root) < folded_before, "压实后应该变小");
    let (_, out, _) = run(&root, &["find", "a.xirang", "第2次"]);
    assert!(out.contains(&id), "压实后值还得在：{out}");

    std::fs::remove_dir_all(&root).ok();
}

/// 「按编号直读」快路：台账可用时 `xr set` 不整份载入（`via_index = true`）；
/// 台账不可用时退回整份载入（`via_index = false`），两条路写出来的结果一样。
#[test]
fn set_uses_the_direct_path_once_the_ledger_is_ready() {
    std::env::remove_var("XIRANG_INDEX_MODE");
    let root = tmp_root("direct");
    let (code, out, err) = run(&root, &["new", "a.xirang", "nil", "根"]);
    assert_eq!(code, 0, "{err}");
    let id = out
        .split('<')
        .nth(1)
        .and_then(|s| s.split('>').next())
        .expect("取节点编号")
        .to_string();

    let path = root.join("a.xirang");
    let f = path.display().to_string();
    let pol = xirang_cli::ops::Policy::cli(false);
    let idv = xirang_core::codec::Uuid::parse(&id).unwrap();

    // 台账已经在（`xr new` 写完就建了）→ 直读那一条记录
    let out = xirang_cli::ops::set_value(
        &pol,
        &xirang_cli::ops::NoHooks,
        &f,
        &id,
        xirang_core::codec::Value::Text("第一次".into()),
        false,
    )
    .unwrap();
    assert!(out.via_index, "台账可用就该直读，不整份载入");
    assert_eq!(idv, out.id);

    // 把台账删掉：快路走不通 → 退回整份载入，并且顺手把台账建回来
    std::fs::remove_dir_all(xirang_core::wsidx::index_dir(&root)).unwrap();
    let out = xirang_cli::ops::set_value(
        &pol,
        &xirang_cli::ops::NoHooks,
        &f,
        &id,
        xirang_core::codec::Value::Text("第二次".into()),
        false,
    )
    .unwrap();
    assert!(!out.via_index, "没有台账时应当退回整份载入");

    // 台账回来了 → 又走快路；两条路写出来的结果一致
    let out = xirang_cli::ops::set_value(
        &pol,
        &xirang_cli::ops::NoHooks,
        &f,
        &id,
        xirang_core::codec::Value::Text("第三次".into()),
        false,
    )
    .unwrap();
    assert!(out.via_index, "台账回来后应当又走直读");
    let (_, out, _) = run(&root, &["find", "a.xirang", "第三次"]);
    assert!(out.contains(&id), "{out}");
    let (code, out, err) = run(&root, &["validate", "a.xirang", "--no-pager"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("0 错误"), "{out}");

    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(xirang_core::wsidx::index_dir(&root)).ok();
}

/// 物理删除（裁剪留痕）必须退回整份重写：追加写表达不了「记录真的没了」。
#[test]
fn prune_still_rewrites_and_keeps_index_honest() {
    let root = tmp_root("prune");
    let (_, out, _) = run(&root, &["new", "a.xirang", "nil", "根"]);
    let id = out
        .split('<')
        .nth(1)
        .and_then(|s| s.split('>').next())
        .expect("取节点编号")
        .to_string();
    for i in 0..8 {
        run(&root, &["set", "a.xirang", &id, &format!("值{i}")]);
    }
    let before = size(&root);
    let (code, _, err) = run(
        &root,
        &["history", "prune", "a.xirang", &id, "--keep", "2", "--yes"],
    );
    assert_eq!(code, 0, "{err}");
    assert!(size(&root) < before, "裁剪留痕要让文件真的变小");
    // 台账不能还记着被删掉的记录
    let (code, out, err) = run(&root, &["index", "status"]);
    assert_eq!(code, 0, "{err}");
    assert!(!out.contains("指纹不符"), "裁剪之后要重新跟上台账：{out}");

    std::fs::remove_dir_all(&root).ok();
}
