//! 桌面端调用的就是 CLI 的同一实现：更新总账、搜索、校验都走 `xirang_cli::ops`。

use std::path::PathBuf;

use xirang_cli::ops::{self, NoHooks, Policy};
use xirang_core::codec::{Uuid, Value};
use xirang_core::tree::Store;

fn tmp_dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("xr_clishared_{name}_{}", Uuid::random_v4()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn update_index_registers_a_never_opened_file() {
    // 侧车模式跳过总账；这个测试专门验「总账」这条路
    std::env::remove_var("XIRANG_INDEX_MODE");
    let dir = tmp_dir("update");
    let path = dir.join("新文件.xirang");
    let mut s = Store::new();
    let root = s.create(None, "根", Value::Empty, false).id;
    s.create(Some(root), "词形", Value::Text("灯".into()), false);
    s.save(&path).unwrap();

    let ws_root = xirang_core::wsidx::workspace_root(&path);
    let idx_dir = xirang_core::wsidx::index_dir(&ws_root);
    let before = idx_dir.exists();

    // 桌面端打开文件时就是调这一句（与 CLI 写完调用的是同一个函数）
    ops::update_index(&path);

    assert!(
        idx_dir.exists(),
        "首次打开后应当生成总账目录（之前存在={before}）"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn search_and_validate_go_through_cli_ops() {
    let dir = tmp_dir("ops");
    let path = dir.join("t.xirang");
    let mut s = Store::new();
    let root = s.create(None, "根", Value::Empty, false).id;
    s.create(Some(root), "词形", Value::Text("灯".into()), false);
    s.create(Some(root), "释义", Value::Text("照明器具".into()), false);
    s.save(&path).unwrap();
    let f = path.display().to_string();
    let pol = Policy::cli(false);

    // 搜索：CLI 的 find（名字 / 值）
    let hits = ops::find(&pol, &NoHooks, &f, "照明").unwrap();
    assert_eq!(hits.len(), 1, "按值搜索命中一条");
    assert_eq!(hits[0].name, "释义");

    // 校验：CLI 的 validate
    let out = ops::validate(&pol, &NoHooks, &f).unwrap();
    assert!(out.errors.is_empty(), "干净文件应当没有错误");
    let _ = std::fs::remove_dir_all(&dir);
}
