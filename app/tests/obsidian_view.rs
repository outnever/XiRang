//! 图视图里的纯函数与数据映射测试（渲染本身要靠肉眼比对，见验收清单）。

use xirang_app::obsidian::physics::{h1, Forces, Physics};
use xirang_app::obsidian::view::{
    get_size, local_subset, node_scale, ru, ru_k, text_alpha, NodeKind, NodeSpec,
};
use xirang_core::codec::Uuid;

fn id(n: u8) -> Uuid {
    Uuid([n; 16])
}

#[test]
fn formulas_match_reference() {
    // RU：0.9 插值
    assert!((ru(0.0, 1.0) - 0.1).abs() < 1e-12);
    assert!((ru(1.0, 0.0) - 0.9).abs() < 1e-12);
    assert!((ru_k(0.0, 1.0, 0.85) - 0.15).abs() < 1e-12);

    // 节点半径：clamp(3√(weight+1), 8, 30)
    assert!((get_size(1.0, 0) - 8.0).abs() < 1e-12, "最小 8");
    assert!((get_size(1.0, 8) - 9.0).abs() < 1e-12, "3√9 = 9");
    assert!((get_size(1.0, 10_000) - 30.0).abs() < 1e-12, "最大 30");
    assert!((get_size(2.0, 8) - 18.0).abs() < 1e-12, "乘上节点大小滑杆");

    // 文字透明度：clamp(log2(scale) + 1 − fade, 0, 1)
    assert!((text_alpha(1.0, 0.0) - 1.0).abs() < 1e-12);
    assert!((text_alpha(0.5, 0.0) - 0.0).abs() < 1e-12);
    assert!((text_alpha(0.25, 0.0) - 0.0).abs() < 1e-12, "再缩也不显示");
    assert!((text_alpha(2.0, 0.0) - 1.0).abs() < 1e-12, "放大封顶 1");
    assert!((text_alpha(1.0, 1.0) - 0.0).abs() < 1e-12, "淡出阈值 +1 就晚一档");
    assert!((text_alpha(2.0, -1.0) - 1.0).abs() < 1e-12, "负阈值更早显示");

    // 节点缩放：√(1/scale)
    assert!((node_scale(0.25) - 2.0).abs() < 1e-12);
}

#[test]
fn slider_mapping_matches_reference() {
    // 参考：默认滑杆 0.5187 → 引擎值 0.1；1.0 → 1.0
    assert!((h1(0.5187, 0.01) - 0.1).abs() < 0.01, "默认中心引力约 0.1");
    assert!((h1(1.0, 0.01) - 1.0).abs() < 1e-12, "拉满 = 1");
    assert!(h1(0.5, 0.01) < h1(0.7, 0.01), "单调递增");

    // 排斥力 = v³，内部存负值
    let mut p = Physics::new();
    let f = Forces {
        center: 0.5187,
        repel: 10.0,
        link: 1.0,
        dist: 250.0,
    };
    f.apply(&mut p);
    assert!((p.repel_strength + 1000.0).abs() < 1e-9, "10³ 且取负");
    assert!((p.link_distance - 250.0).abs() < 1e-12);
    assert!(p.center_strength > 0.09 && p.center_strength < 0.11);
}

#[test]
fn local_subset_keeps_one_hop_and_weights_center() {
    let specs = vec![
        spec(1, "中心"),
        spec(2, "前向"),
        spec(3, "反向"),
        spec(4, "两跳外"),
    ];
    let edges = vec![(id(1), id(2)), (id(3), id(1)), (id(4), id(2))];
    let (nodes, links) = local_subset(&specs, &edges, id(1));
    assert_eq!(nodes.len(), 3, "中心 + 1 跳（前向 / 反向）");
    let center = nodes.iter().find(|n| n.id == id(1)).unwrap();
    assert_eq!(center.weight, 30, "中心权重 30（照抄 Obsidian）");
    assert!(center.kind == NodeKind::Focused);
    let hop = nodes.iter().find(|n| n.id == id(2)).unwrap();
    assert_eq!(hop.weight, 0, "1 跳权重 0");
    assert_eq!(links.len(), 2, "只保留与中心相连的边");
    assert!(links.iter().all(|(a, b)| *a == id(1) || *b == id(1)));
    assert!(!nodes.iter().any(|n| n.id == id(4)), "两跳外的不进局部图");
}

fn spec(n: u8, title: &str) -> NodeSpec {
    NodeSpec {
        id: id(n),
        title: title.into(),
        kind: NodeKind::Note,
        weight: 0,
        series: None,
        file: "t.xirang".into(),
    }
}

#[test]
fn physics_thread_feeds_positions() {
    // 端到端：建两个相连节点 → 后台线程推几步 → 位置被拉到 linkDistance 附近
    use xirang_app::obsidian::view::{GraphView, NodeSpec};
    let mut g = GraphView::new();
    g.opt.dist = 100.0;
    g.set_data(
        vec![
            NodeSpec {
                id: id(1),
                title: "甲".into(),
                kind: NodeKind::Note,
                weight: 1,
                series: None,
                file: "t".into(),
            },
            NodeSpec {
                id: id(2),
                title: "乙".into(),
                kind: NodeKind::Note,
                weight: 1,
                series: None,
                file: "t".into(),
            },
        ],
        vec![(id(1), id(2))],
    );
    std::thread::sleep(std::time::Duration::from_millis(400));
    assert_eq!(g.node_count(), 2);
    assert_eq!(g.link_count(), 1);
}

/// 「按根聚合」：引用边折算成**根之间**的边（对应 Obsidian 的「一篇笔记 = 一个节点」）。
#[test]
fn root_edges_aggregate_by_top_level_root() {
    use xirang_app::lazy::Doc;
    use xirang_core::codec::Value;
    use xirang_core::index::sidecar_path;
    use xirang_core::tree::Store;

    let path = std::env::temp_dir().join(format!("xr_rootedges_{}.xirang", Uuid::random_v4()));
    let mut s = Store::new();
    // 两个词条（根），各自一个节点指向对方 → 应当得到一条「根 ↔ 根」的边
    let a = s.create(None, "词条甲", Value::Empty, false).id;
    let b = s.create(None, "词条乙", Value::Empty, false).id;
    let a_child = s.create(Some(a), "指向乙", Value::Empty, false).id;
    let b_child = s.create(Some(b), "指向甲", Value::Empty, false).id;
    s.update(a_child, Value::Reference(b_child)).unwrap();
    s.update(b_child, Value::Reference(a_child)).unwrap();
    // 同一根内部的自引用：不该出现在根图上
    let inner = s.create(Some(a), "自引用", Value::Reference(a_child), false).id;
    let _ = inner;
    s.save(&path).unwrap();

    let mut doc = Doc::open(&path).unwrap();
    let edges = doc.root_edges();
    assert_eq!(edges.len(), 2, "两条跨根引用都会保留");
    assert!(edges.iter().all(|(x, y)| (*x == a && *y == b) || (*x == b && *y == a)));
    // 按节点的原始边更多（含根内自引用）
    let raw = doc.edges();
    assert!(raw.len() > edges.len(), "根聚合会丢掉根内的自引用");
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(sidecar_path(&path));
}
