//! 跨文件身份的常驻护栏：同一个编号在多个文件里时，**读并集、写同步、冲突先裁决**。

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn tmp_root(tag: &str) -> PathBuf {
    let n = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let d = std::env::temp_dir().join(format!("xr-shared-{tag}-{}-{n}", std::process::id()));
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

fn grab(out: &str) -> String {
    out.split('<')
        .nth(1)
        .and_then(|s| s.split('>').next())
        .expect("取编号")
        .to_string()
}

/// 造两个文件：**同一个编号**（词形）在两边、三字段一致，各自挂不同的孩子。
fn two_files(root: &Path) -> (String, String) {
    let (code, out, err) = run(root, &["new", "a.xirang", "nil", "根"]);
    assert_eq!(code, 0, "{err}");
    let root_id = grab(&out);
    let (_, out, _) = run(root, &["new", "a.xirang", &root_id, "词形", "灯"]);
    let word = grab(&out);
    let (_, out, _) = run(root, &["new", "a.xirang", &word, "甲的孩子", "x"]);
    assert!(!out.is_empty());
    std::fs::copy(root.join("a.xirang"), root.join("b.xirang")).unwrap();
    // 复制出来的 b 还没登记；rebuild 顺手把两份都登记上
    let (code, _, err) = run(root, &["index", "rebuild", "a.xirang", "b.xirang"]);
    assert_eq!(code, 0, "{err}");
    (word, root_id)
}

#[test]
fn set_syncs_all_files_and_here_opts_out() {
    let root = tmp_root("sync");
    let (word, _) = two_files(&root);

    // 默认：两份都改
    let (code, out, err) = run(&root, &["set", "a.xirang", &word, "火"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("同步 2 个文件"), "要报「同步了几个文件」：{out}");
    for f in ["a.xirang", "b.xirang"] {
        let (_, out, _) = run(&root, &["find", f, "火"]);
        assert!(out.contains(&word), "{f} 应当也改成「火」：{out}");
    }

    // --here：只改点名的那个，并提示别处没跟着改
    let (code, out, err) = run(&root, &["set", "b.xirang", &word, "水", "--here"]);
    assert_eq!(code, 0, "{err}");
    assert!(!out.contains("同步 2 个文件"), "--here 不该报同步：{out}");
    assert!(err.contains("别处还有副本没跟着改"), "{err}");
    let (_, out, _) = run(&root, &["find", "a.xirang", "火"]);
    assert!(out.contains(&word), "a 应当还是「火」：{out}");

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn conflict_refuses_write_and_check_helps_resolve_it() {
    let root = tmp_root("conflict");
    let (word, _) = two_files(&root);

    // 只在 b 里改名字 → 两边「名字」不一致
    let (code, _, err) = run(&root, &["rename", "b.xirang", &word, "词形改", "--here"]);
    assert_eq!(code, 0, "{err}");

    // 读：照常并集展示，但要标注自身不一致
    let (code, out, err) = run(&root, &["ws", &word, "a.xirang", "b.xirang"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("2 处"), "{out}");
    assert!(out.contains("自身不一致"), "读时要标注冲突：{out}");
    assert!(err.contains("catalog check"), "stderr 要给裁决提示：{err}");

    // 体检：默认信息性（退出码 0），--strict 才非 0，--json 可解析
    let (code, out, _) = run(&root, &["catalog", "check"]);
    assert_eq!(code, 0, "默认应当是信息性：{out}");
    assert!(out.contains(&word), "{out}");
    let (code, out, _) = run(&root, &["catalog", "check", "--strict"]);
    assert_eq!(code, 2, "--strict 有冲突应当退出码 2：{out}");
    let (code, out, _) = run(&root, &["catalog", "check", "--json"]);
    assert_eq!(code, 0);
    assert!(out.contains("\"conflicts\"") && out.contains(&word), "{out}");

    // 写：拒绝（先裁决）
    let (code, _, err) = run(&root, &["set", "a.xirang", &word, "火"]);
    assert_eq!(code, 2, "冲突时不该写：{err}");
    assert!(err.contains("先裁决"), "要说清先裁决再写：{err}");

    // 裁决：以 a 为基准对齐 → 之后又能同步
    let (code, out, err) =
        run(&root, &["catalog", "check", "--sync", &word, "--base", "a.xirang"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("已同步"), "{out}");
    let (code, _, err) = run(&root, &["set", "a.xirang", &word, "火"]);
    assert_eq!(code, 0, "裁决之后应当能同步写：{err}");
    for f in ["a.xirang", "b.xirang"] {
        let (_, out, _) = run(&root, &["find", f, "火"]);
        assert!(out.contains(&word), "{f} 应当一起改成「火」：{out}");
    }

    std::fs::remove_dir_all(&root).ok();
}

/// 批量提交也按「改一处同步到所有副本」：默认两份都改；`--here` 才只改点名的那个。
#[test]
fn batch_syncs_all_copies_and_here_opts_out() {
    let root = tmp_root("batchsync");
    let (word, _) = two_files(&root);
    std::fs::write(
        root.join("l.jsonl"),
        format!("{{\"op\":\"set\",\"id\":\"{word}\",\"value\":\"批量\"}}\n"),
    )
    .unwrap();

    // 默认：两份都改，并逐文件报账
    let (code, out, err) = run(&root, &["batch", "a.xirang", "l.jsonl", "--no-history"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("已批量提交"), "{out}");
    assert_eq!(out.matches(".xirang →").count(), 2, "逐文件报账：{out}");
    for f in ["a.xirang", "b.xirang"] {
        let (_, out, _) = run(&root, &["find", f, "批量"]);
        assert!(out.contains(&word), "{f} 应当也改成「批量」：{out}");
    }

    // --here：只改点名的那个，并提示别处没跟着改
    std::fs::write(
        root.join("l2.jsonl"),
        format!("{{\"op\":\"set\",\"id\":\"{word}\",\"value\":\"只改一份\"}}\n"),
    )
    .unwrap();
    let (code, _out, err) =
        run(&root, &["batch", "a.xirang", "l2.jsonl", "--no-history", "--here"]);
    assert_eq!(code, 0, "{err}");
    assert!(err.contains("副本没跟着改"), "{err}");
    let (_, out, _) = run(&root, &["find", "b.xirang", "只改一份"]);
    assert!(out.contains("0 个匹配"), "b 不该被改：{out}");

    std::fs::remove_dir_all(&root).ok();
}

/// `xr new --files a,b`：**同一个编号**一次写进两个文件；之后改它两份一起改。
#[test]
fn new_with_files_creates_one_identity_in_several_files() {
    let root = tmp_root("newfiles");
    let (code, out, err) = run(&root, &["new", "a.xirang", "nil", "根"]);
    assert_eq!(code, 0, "{err}");
    let root_id = grab(&out);
    std::fs::copy(root.join("a.xirang"), root.join("b.xirang")).unwrap();
    let (code, _, err) = run(&root, &["index", "rebuild", "a.xirang", "b.xirang"]);
    assert_eq!(code, 0, "{err}");

    // 一次新建、写进两份（同一个编号）
    let (code, out, err) = run(
        &root,
        &["new", "a.xirang", &root_id, "共享词条", "灯", "--files", "a.xirang,b.xirang"],
    );
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("写进 2 个文件"), "{out}");
    let new_id = grab(&out);

    let (code, out, err) = run(&root, &["ws", &new_id, "a.xirang", "b.xirang"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("2 处"), "两份里应当是同一个编号：{out}");

    // 之后改它 → 两份一起改（沿用同步规则）
    let (code, _, err) = run(&root, &["set", "a.xirang", &new_id, "火"]);
    assert_eq!(code, 0, "{err}");
    let (_, out, _) = run(&root, &["find", "b.xirang", "火"]);
    assert!(out.contains(&new_id), "b 应当也改成「火」：{out}");

    std::fs::remove_dir_all(&root).ok();
}

/// `--files` 指向一个还不存在的文件 → 整份新建（带文件头）。
#[test]
fn new_with_files_can_create_a_new_file() {
    let root = tmp_root("newfilecreate");
    let (code, out, err) =
        run(&root, &["new", "a.xirang", "nil", "新根", "--files", "a.xirang"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("已创建"), "{out}");
    let (code, out, err) = run(&root, &["info", "a.xirang"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("节点数: 1"), "新文件应当只有这个根节点：{out}");
    std::fs::remove_dir_all(&root).ok();
}

/// 跨文件引用：连边不该被拦，校验要认（别处有就算通过），真悬空的仍要报。
#[test]
fn validate_and_link_accept_cross_file_references() {
    let root = tmp_root("xvalidate");
    // 乙：目标
    let (_, out, _) = run(&root, &["new", "b.xirang", "nil", "通用词库"]);
    let b_root = grab(&out);
    let (_, out, _) = run(&root, &["new", "b.xirang", &b_root, "一不做二不休"]);
    let target = grab(&out);
    // 甲：引用指向乙里的目标
    let (_, out, _) = run(&root, &["new", "a.xirang", "nil", "成语词库"]);
    let a_root = grab(&out);
    let (_, out, _) = run(&root, &["new", "a.xirang", &a_root, "典故"]);
    let r = grab(&out);

    // 目标在别的文件里，连边也不该被拦（不加 --yes）
    let (code, _, err) = run(&root, &["link", "a.xirang", &r, &target]);
    assert_eq!(code, 0, "跨文件连边不该被拦：{err}");
    let (code, _, err) = run(&root, &["index", "rebuild", "a.xirang", "b.xirang"]);
    assert_eq!(code, 0, "{err}");

    let (code, out, err) = run(&root, &["validate", "a.xirang"]);
    assert_eq!(code, 0, "跨文件引用不该报断裂：{out}{err}");
    assert!(out.contains("0 错误"), "{out}");

    // 真·悬空引用仍要报 R001
    let (_, out, _) = run(&root, &["new", "a.xirang", &a_root, "假引用"]);
    let bogus = grab(&out);
    let _ = run(
        &root,
        &["link", "a.xirang", &bogus, "99999999-1111-4222-8333-444444444444", "--yes"],
    );
    let (code, out, _) = run(&root, &["validate", "a.xirang"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("R001"), "真悬空引用仍要报：{out}");

    std::fs::remove_dir_all(&root).ok();
}
