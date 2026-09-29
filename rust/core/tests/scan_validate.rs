//! 流式校验的跨文件口径 + `@模板` 容器误用的只读提示。

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use xirang_core::codec::{Uuid, Value};
use xirang_core::scan;
use xirang_core::tree::Store;

fn tmp(name: &str) -> PathBuf {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("xr-scanval-{name}-{}-{n}.xirang", std::process::id()))
}

/// 引用目标在别的文件里时（跨文件身份），校验不该报 R001——由调用方给的
/// `external` 口径说了算（CLI 那边 = 工作区台账 ∪ 本机目录）。
#[test]
fn cross_file_reference_is_not_a_break_when_the_oracle_says_it_exists() {
    let p = tmp("xref");
    let mut s = Store::new();
    let root = s.create(None, "词库", Value::Empty, false).id;
    let ghost = Uuid::random_v4(); // 目标不在本文件
    s.create(Some(root), "指向别处", Value::Reference(ghost), false);
    s.save(&p).unwrap();

    let cancel = AtomicBool::new(false);
    let mut progress = |_: u64| {};
    // 别处也找不到 → 真断裂
    let rep = scan::validate_stream_ext(&p, &cancel, &mut progress, &mut |_| false).unwrap();
    assert!(rep.issues.iter().any(|i| i.code == "R001"), "{:?}", rep.issues);
    // 别处有这个编号（跨文件）→ 不算断裂
    let rep = scan::validate_stream_ext(&p, &cancel, &mut progress, &mut |t| t == ghost).unwrap();
    assert!(!rep.issues.iter().any(|i| i.code == "R001"), "{:?}", rep.issues);

    std::fs::remove_file(&p).ok();
}

/// `@模板`(空) 本该是叶子标记；下面还挂普通子节点 → 只读提示改用 `模板集`（不当错误）。
#[test]
fn warns_when_the_template_marker_is_used_as_a_container() {
    let cancel = AtomicBool::new(false);
    let mut progress = |_: u64| {};

    // 误用：`@模板`(空) 当容器
    let p = tmp("tplcontainer");
    let mut s = Store::new();
    let root = s.create(None, "词库", Value::Empty, false).id;
    let tpl = s.create(Some(root), "@模板", Value::Empty, false).id;
    s.create(Some(tpl), "词条模板", Value::Empty, false);
    s.save(&p).unwrap();
    let rep = scan::validate_stream_ext(&p, &cancel, &mut progress, &mut |_| false).unwrap();
    assert!(
        rep.warnings.iter().any(|w| w.message.contains("模板集")),
        "应当提示改用 模板集：{:?}",
        rep.warnings
    );
    assert!(rep.issues.is_empty(), "只读提示不该算错误：{:?}", rep.issues);
    std::fs::remove_file(&p).ok();

    // 正规：标记是叶子，结构挂在模板根下（不是挂在标记下）→ 不提示
    let q = tmp("tplok");
    let mut s = Store::new();
    let root = s.create(None, "词库", Value::Empty, false).id;
    let tpl = s.create(Some(root), "词条模板", Value::Empty, false).id;
    s.create(Some(tpl), "@模板", Value::Empty, false);
    s.create(Some(tpl), "词条名", Value::Empty, false);
    s.save(&q).unwrap();
    let rep = scan::validate_stream_ext(&q, &cancel, &mut progress, &mut |_| false).unwrap();
    assert!(rep.warnings.is_empty(), "正规模板不该被提示：{:?}", rep.warnings);
    std::fs::remove_file(&q).ok();
}
