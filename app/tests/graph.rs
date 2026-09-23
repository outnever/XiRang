//! 引用图与内存释放的测试（都不需要界面）。

use std::path::PathBuf;

use xirang_app::graph::{Graph, Settings};
use xirang_app::lazy::Doc;
use xirang_core::codec::{Uuid, Value};
use xirang_core::index::sidecar_path;
use xirang_core::tree::Store;

/// 单文件建图的便捷封装：候选带同一个文件名，解析走该文件的 Doc。
fn build_graph(g: &mut Graph, doc: &mut Doc, ids: &[Uuid], show_aux: bool) {
    let candidates: Vec<(Uuid, String)> = ids
        .iter()
        .map(|id| (*id, "sample.xirang".to_string()))
        .collect();
    let mut resolve = |id: Uuid| doc.node(id).map(|n| (n, "sample.xirang".to_string()));
    g.build(&candidates, show_aux, &mut resolve);
}

fn tmp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("xr_graph_{name}_{}.xirang", Uuid::random_v4()))
}

fn cleanup(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(sidecar_path(path));
}

/// 用来测试的树：甲 → 指向乙 → 深层（也指向乙）；丙 → 也指向乙；另有孤立的一点。
struct Ids {
    link: Uuid,
    deep: Uuid,
    target: Uuid,
}

fn sample(path: &std::path::Path) -> Ids {
    let mut s = Store::new();
    let a = s.create(None, "甲", Value::Empty, false).id;
    let b = s.create(None, "乙", Value::Empty, false).id;
    let c = s.create(None, "丙", Value::Empty, false).id;
    let link = s.create(Some(a), "指向乙", Value::Reference(b), false).id;
    let deep = s.create(Some(link), "深层", Value::Reference(b), false).id;
    s.create(Some(c), "也指向乙", Value::Reference(b), false);
    let lonely = s.create(None, "孤立", Value::Empty, false).id;
    s.create(Some(lonely), "没人引用我", Value::Text("x".into()), false);
    s.save(path).unwrap();
    Ids { link, deep, target: b }
}

/// 建图时的候选集合：整棵树走一遍（界面上是「已展开的那些节点」）。
fn candidates(doc: &mut Doc) -> Vec<Uuid> {
    let mut out = Vec::new();
    let mut queue = doc.roots();
    while let Some(id) = queue.pop() {
        if out.len() > 10_000 {
            break;
        }
        out.push(id);
        queue.extend(doc.children(id));
    }
    out
}

#[test]
fn graph_keeps_only_connected_nodes_by_default() {
    let p = tmp("connected");
    sample(&p);
    let mut doc = Doc::open(&p).unwrap();
    let cands = candidates(&mut doc);

    let mut g = Graph::new(Settings::default());
    assert_eq!(g.settings.link_distance, 250.0, "默认值照搬 Obsidian");
    assert_eq!(g.settings.center_strength, 0.1);
    assert_eq!(g.settings.repel_strength, 10.0);
    build_graph(&mut g, &mut doc, &cands, true);
    assert_eq!(g.len(), 4, "指向乙 / 深层 / 也指向乙 / 乙（目标只算一次）");
    assert_eq!(g.edges.len(), 3);
    assert!(g.nodes.iter().all(|n| n.name != "孤立"), "默认不画孤立节点");

    // 打开「显示孤立节点」后多出来
    let mut g2 = Graph::new(Settings {
        show_orphans: true,
        ..Settings::default()
    });
    build_graph(&mut g2, &mut doc, &cands, true);
    assert!(g2.nodes.iter().any(|n| n.name == "孤立"));

    // 关掉辅助节点：@ 开头的节点不进图
    let mut s = Store::load_view(&p).unwrap();
    let target = s.roots()[0].id;
    s.create(Some(target), "@note", Value::Reference(target), false);
    s.save(&p).unwrap();
    let mut doc = Doc::open(&p).unwrap();
    let cands = candidates(&mut doc);
    let mut g3 = Graph::new(Settings::default());
    build_graph(&mut g3, &mut doc, &cands, false);
    assert!(g3.nodes.iter().all(|n| !n.name.starts_with('@')));
    cleanup(&p);
}

#[test]
fn force_step_pulls_linked_nodes_together_and_separates_others() {
    let p = tmp("force");
    sample(&p);
    let mut doc = Doc::open(&p).unwrap();
    let cands = candidates(&mut doc);
    let mut g = Graph::new(Settings {
        link_distance: 60.0,
        ..Settings::default()
    });
    build_graph(&mut g, &mut doc, &cands, true);

    let dist = |g: &Graph, i: usize, j: usize| {
        let (a, b) = (&g.nodes[i], &g.nodes[j]);
        ((a.pos[0] - b.pos[0]).powi(2) + (a.pos[1] - b.pos[1]).powi(2)).sqrt()
    };
    let (i, j) = g.edges[0];
    let before = dist(&g, i, j);
    for _ in 0..200 {
        g.step(1);
    }
    let after = dist(&g, i, j);
    assert!(
        (after - 60.0).abs() < before.max(60.0) * 0.6,
        "连线长度应趋近 link_distance：{before:.1} → {after:.1}"
    );
    // 不同节点的圆不该重叠
    for a in 0..g.nodes.len() {
        for b in (a + 1)..g.nodes.len() {
            let min = g.settings.radius(g.nodes[a].degree) + g.settings.radius(g.nodes[b].degree);
            assert!(
                dist(&g, a, b) > min - 6.0,
                "节点 {a}/{b} 挤在一起了：{:.1}",
                dist(&g, a, b)
            );
        }
    }
    cleanup(&p);
}

#[test]
fn simulation_stops_when_idle_and_wakes_on_drag() {
    let p = tmp("idle");
    sample(&p);
    let mut doc = Doc::open(&p).unwrap();
    let cands = candidates(&mut doc);
    let mut g = Graph::new(Settings::default());
    build_graph(&mut g, &mut doc, &cands, true);
    let mut moved = false;
    for _ in 0..600 {
        moved = g.step(1);
    }
    assert!(!moved, "静止后不再迭代（省电）");
    assert_eq!(g.alpha, 0.0);

    g.set_dragging(true);
    assert!(g.alpha >= 0.5, "拖动时把模拟唤醒");
    assert_eq!(g.alpha_target, 0.3, "对应 Obsidian 拖拽时的 alphaTarget");
    g.set_dragging(false);
    assert_eq!(g.alpha_target, 0.0);
    cleanup(&p);
}

#[test]
fn label_lod_follows_zoom_and_multiplier() {
    let s = Settings::default();
    assert_eq!(s.label_threshold(), 1.0, "默认阈值 = 1（原样缩放才出现名字）");
    assert_eq!(s.label_alpha(0.2), 0.0, "缩小到一定程度不显示名字");
    assert_eq!(s.label_alpha(2.0), 1.0, "放大后完整显示");
    assert!(s.label_alpha(0.8) > 0.0 && s.label_alpha(0.8) < 1.0, "中间是渐入");

    let early = Settings {
        text_fade_multiplier: -1.5,
        ..Settings::default()
    };
    let late = Settings {
        text_fade_multiplier: 1.5,
        ..Settings::default()
    };
    assert!(early.label_threshold() < 1.0, "负值 = 更早显示名字");
    assert!(late.label_threshold() > 1.0, "正值 = 要放更大才显示");
    assert!(early.label_alpha(0.6) > late.label_alpha(0.6));
}

#[test]
fn focus_puts_subtree_on_a_tree_layout() {
    let p = tmp("focus");
    let ids = sample(&p);
    let mut doc = Doc::open(&p).unwrap();
    let cands = candidates(&mut doc);
    let mut g = Graph::new(Settings::default());
    build_graph(&mut g, &mut doc, &cands, true);

    let members: Vec<Uuid> = g.nodes.iter().map(|n| n.id).collect();
    let subtree = Graph::subtree(&mut doc, ids.link, &members);
    assert_eq!(subtree[0], (ids.link, 0), "焦点自己在第 0 层");
    let child = subtree
        .iter()
        .find(|(id, _)| *id == ids.deep)
        .expect("焦点下面那层也在图里");
    assert_eq!(child.1, 1, "孩子在第 1 层");
    assert_eq!(
        subtree.iter().find(|(id, _)| *id == ids.target).map(|(_, d)| *d),
        None,
        "乙 只是引用目标，父链不在焦点子树里，所以不在树布局里"
    );

    let targets = g.focus_targets(&subtree, [0.0, 0.0], 130.0, 34.0);
    let root_pos = targets[&ids.link];
    let child_pos = targets[&child.0];
    assert!(child_pos[0] > root_pos[0], "树布局：孩子往右一层");
    assert_ne!(child_pos[1], root_pos[1], "同一层的行错开，不重叠");

    // 朝目标位置过渡：应该越走越近
    let before = g.nodes[g.index_of(ids.link).unwrap()].pos;
    let d0 = ((before[0] - root_pos[0]).powi(2) + (before[1] - root_pos[1]).powi(2)).sqrt();
    for _ in 0..20 {
        g.move_towards(&targets, 0.25);
    }
    let after = g.nodes[g.index_of(ids.link).unwrap()].pos;
    let d1 = ((after[0] - root_pos[0]).powi(2) + (after[1] - root_pos[1]).powi(2)).sqrt();
    assert!(d1 < d0, "过渡动画朝目标走：{d0:.1} → {d1:.1}");
    cleanup(&p);
}

#[test]
fn cache_can_be_released_and_trimmed() {
    let p = tmp("memory");
    let ids0 = sample(&p);
    let mut doc = Doc::open(&p).unwrap();
    // 读几个节点进缓存
    let _ = doc.node(ids0.link);
    let _ = doc.node(ids0.deep);
    let _ = doc.node(ids0.target);
    assert!(doc.cache_len() > 0);
    assert!(doc.cache_bytes() > 0);

    // 只留 1 个
    doc.trim_cache(1);
    assert_eq!(doc.cache_len(), 1);

    // 全清
    doc.clear_cache();
    assert_eq!(doc.cache_len(), 0);
    assert_eq!(doc.cache_bytes(), 0);

    // 清掉之后还能按索引重新读回来（说明缓存只是缓存）
    let again = doc.node(ids0.link).unwrap();
    assert_eq!(again.id, ids0.link);
    cleanup(&p);
}
