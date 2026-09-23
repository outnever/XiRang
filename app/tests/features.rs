//! 导出范围、中英文案、配色、图片解码的测试（都不需要界面）。

use std::collections::HashSet;
use std::path::PathBuf;

use xirang_app::blobimg::{self, Kind};
use xirang_app::export::{self, Scope};
use xirang_app::i18n::{self, Lang};
use xirang_app::lazy::Doc;
use xirang_app::theme::{self, Palette};
use xirang_core::codec::{Uuid, Value};
use xirang_core::index::sidecar_path;
use xirang_core::tree::Store;

fn tmp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("xr_feat_{name}_{}.xirang", Uuid::random_v4()))
}

fn cleanup(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(sidecar_path(path));
}

/// 根 → 甲 → 甲1；根 → 乙
fn sample(path: &std::path::Path) -> (Uuid, Uuid, Uuid, Uuid) {
    let mut s = Store::new();
    let root = s.create(None, "灯", Value::Empty, false).id;
    let a = s.create(Some(root), "甲", Value::Empty, false).id;
    let a1 = s.create(Some(a), "甲1", Value::Text("x".into()), false).id;
    let b = s.create(Some(root), "乙", Value::Empty, false).id;
    s.save(path).unwrap();
    (root, a, a1, b)
}

#[test]
fn export_full_view_and_subtree() {
    let p = tmp("export");
    let (root, a, a1, b) = sample(&p);
    let mut doc = Doc::open(&p).unwrap();

    // 完整折叠视图 = 全部 4 个节点
    let full = export::build(&mut doc, &Scope::Full, &HashSet::new()).unwrap();
    assert_eq!(full.len(), 4);

    // 当前视图：只展开了根与「甲」→ 甲1 在内、但「甲」折叠时不含甲1
    let mut expanded: HashSet<Uuid> = HashSet::new();
    expanded.insert(root);
    expanded.insert(a);
    let view = export::build(&mut doc, &Scope::View, &expanded).unwrap();
    assert_eq!(view.len(), 4, "根 + 甲 + 甲1 + 乙");

    let mut shallow: HashSet<Uuid> = HashSet::new();
    shallow.insert(root); // 只展开根
    let view = export::build(&mut doc, &Scope::View, &shallow).unwrap();
    assert_eq!(view.len(), 3, "甲折叠着 → 不含甲1");
    assert!(view.get(a1).is_none());
    assert!(view.get(b).is_some());

    // 子树：只要「甲」这一支
    let sub = export::build(&mut doc, &Scope::Subtree(a), &expanded).unwrap();
    assert_eq!(sub.len(), 2, "甲 + 甲1");
    assert!(sub.get(a).is_some());
    assert!(sub.get(a1).is_some());
    assert!(sub.get(root).is_none(), "父节点不跟着导出");
    assert!(sub.get(b).is_none());

    // 文本导出能跑通四种格式
    for fmt in ["json", "xml", "yaml", "md"] {
        let text = export::to_text(&sub, fmt);
        assert!(!text.is_empty(), "{fmt} 导出不该是空的");
        assert!(text.contains("甲1"), "{fmt} 里应当有子树内容");
    }
    cleanup(&p);
}

#[test]
fn i18n_switches_and_covers_every_label_used_in_ui() {
    i18n::set(Lang::Zh);
    assert_eq!(i18n::t("打开…"), "打开…");
    i18n::set(Lang::En);
    assert_eq!(i18n::t("打开…"), "Open…");
    assert_eq!(i18n::t("引用图"), "Graph");

    // 守护：界面代码里 t("…") 用到的每个串都必须在表里（漏翻会红）
    let ui = include_str!("../src/main.rs");
    let mut missing = Vec::new();
    let bytes = ui.as_bytes();
    let mut rest = ui;
    while let Some(pos) = rest.find("t(\"") {
        // 只看真正的 t("…") 调用：前一个字符必须是 ( , = { 之一（排除 after_edit( / .text( / from_id_salt(）
        let abs = ui.len() - rest.len() + pos;
        let ok = abs > 0 && matches!(bytes[abs - 1], b'(' | b',' | b'=' | b'{' | b' ');
        rest = &rest[pos + 3..];
        if let Some(end) = rest.find('"') {
            let key = &rest[..end];
            if ok && !i18n::keys().contains(&key) {
                missing.push(key.to_string());
            }
            rest = &rest[end..];
        }
    }
    assert!(
        missing.is_empty(),
        "这些界面文案没有英文：{missing:?}（补进 i18n::TABLE）"
    );
    i18n::set(Lang::Zh);
}

#[test]
fn palette_hex_roundtrip_and_visuals() {
    let c = eframe::egui::Color32::from_rgb(0x09, 0x69, 0xda);
    assert_eq!(theme::parse_hex(&theme::to_hex(c)), Some(c));
    assert_eq!(theme::parse_hex("not-a-color"), None);

    let p = Palette::light();
    let v = theme::visuals(&p);
    assert_eq!(v.panel_fill, theme::parse_hex(&p.bg).unwrap());
    assert_eq!(v.selection.bg_fill, theme::parse_hex(&p.accent).unwrap());
    assert!(v.override_text_color.is_some());

    let custom = Palette {
        dark: true,
        bg: "#101010".into(),
        fg: "#f0f0f0".into(),
        accent: "#ff0000".into(),
        aux: "#00ff00".into(),
        border: "#222222".into(),
    };
    let v2 = theme::visuals(&custom);
    assert_eq!(v2.panel_fill, eframe::egui::Color32::from_rgb(0x10, 0x10, 0x10));
    assert_eq!(v2.selection.bg_fill, eframe::egui::Color32::from_rgb(0xff, 0, 0));
    assert_eq!(theme::aux_color(&custom), eframe::egui::Color32::from_rgb(0, 0xff, 0));
}

#[test]
fn blob_image_sniff_and_decode() {
    // 造一张 2×2 的 PNG（红、绿、蓝、白）
    let mut png_bytes: Vec<u8> = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut png_bytes, 2, 2);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().unwrap();
        writer
            .write_image_data(&[
                255, 0, 0, 255, //
                0, 255, 0, 255, //
                0, 0, 255, 255, //
                255, 255, 255, 255,
            ])
            .unwrap();
    }
    assert_eq!(blobimg::sniff(&png_bytes), Kind::Png);
    let (w, h, rgba) = blobimg::decode(&png_bytes).expect("PNG 应当能解码");
    assert_eq!((w, h), (2, 2));
    assert_eq!(&rgba[0..4], &[255, 0, 0, 255]);
    assert_eq!(&rgba[12..16], &[255, 255, 255, 255]);

    // JPEG：只验魔数（没有编码器，不造样本）
    assert_eq!(blobimg::sniff(&[0xff, 0xd8, 0xff, 0xe0, 0, 0]), Kind::Jpeg);
    // 不认识的字节返回 None，界面会退回十六进制预览
    assert_eq!(blobimg::sniff(b"hello"), Kind::Other);
    assert!(blobimg::decode(b"hello").is_none());
    assert!(blobimg::decode(&png_bytes[..20]).is_none(), "截断的 PNG 不该 panic");
}
