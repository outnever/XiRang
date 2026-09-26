//! 桌面端编辑必须与 CLI **共用同一套落盘规则**（`xirang_core::edit`）：
//! 只追加、幂等补 `@protocol = append-v1`、写前截尾部残片，索引走
//! `xirang_cli::ops::update_index` 这同一个入口。撤销栈是界面自己的东西，
//! 与数据里的 `@history` 分开。

use std::path::PathBuf;

use xirang_app::edit;
use xirang_cli::ops::{self, NoHooks, Policy};
use xirang_core::codec::{Uuid, Value};
use xirang_core::tree::{self, Store};

fn tmp_file(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("xr_app_edit_{}_{}", name, Uuid::random_v4()));
    std::fs::create_dir_all(&d).unwrap();
    let path = d.join("a.xirang");
    let mut s = Store::new();
    let root = s.create(None, "根", Value::Empty, false).id;
    s.create(Some(root), "词形", Value::Text("灯".into()), false);
    s.save(&path).unwrap();
    path
}

fn child(path: &std::path::Path, name: &str) -> xirang_core::codec::Node {
    tree::Store::load_view(path)
        .unwrap()
        .nodes()
        .iter()
        .find(|n| n.name == name)
        .cloned()
        .expect("找得到")
}

#[test]
fn desktop_edit_and_cli_agree_on_append_semantics() {
    std::env::remove_var("XIRANG_INDEX_MODE");
    let path = tmp_file("agree");
    let word = child(&path, "词形");
    let root = tree::Store::load_view(&path).unwrap().root_of(word.id).unwrap();

    // 桌面端：改一个词（界面里的撤销栈照旧）
    let mut ed = edit::Editor::open(&path).unwrap();
    let e = edit::set_value(&word, Value::Text("火".into()), Some(root));
    ed.apply(e).unwrap();
    // 界面每次编辑后调的就是这一句
    ops::update_index(&path);

    // CLI 读得到的值就是它（同一个操作层）
    let pol = Policy::cli(false);
    let f = path.display().to_string();
    let hits = ops::find(&pol, &NoHooks, &f, "火").unwrap();
    assert_eq!(hits.len(), 1, "CLI 应该看得到桌面端刚写进去的值");

    // 撤销：追加「前态」，值回到「灯」——同样是只追加
    assert!(ed.undo().unwrap());
    ops::update_index(&path);
    assert_eq!(
        tree::Store::load_view(&path).unwrap().get(word.id).unwrap().value,
        Value::Text("灯".into())
    );

    // 面板里那条 `@protocol = append-v1` 只该有一条（幂等，跨会话也算）
    let text = format!("{:?}", tree::Store::load_view(&path).unwrap().nodes());
    assert_eq!(text.matches("append-v1").count(), 1, "协议声明必须幂等：{text}");
    let mut ed2 = edit::Editor::open(&path).unwrap();
    ed2.apply(edit::set_value(&word, Value::Text("再改".into()), Some(root))).unwrap();
    ops::update_index(&path); // 界面每次编辑后都调它
    let text = format!("{:?}", tree::Store::load_view(&path).unwrap().nodes());
    assert_eq!(text.matches("append-v1").count(), 1, "重开编辑器也不该再加一条：{text}");

    // 台账跟得上（桌面端读的就是它）
    let ws_root = xirang_core::wsidx::workspace_root(&path);
    let st = xirang_core::wsidx::files_status(&ws_root).unwrap();
    assert!(st.iter().all(|f| f.fresh), "编辑后台账要新鲜：{st:?}");

    let _ = std::fs::remove_dir_all(path.parent().unwrap());
    let _ = std::fs::remove_dir_all(xirang_core::wsidx::index_dir(&ws_root));
}
