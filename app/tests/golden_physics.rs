//! 与 JS 参考实现的**逐位对拍**：这是「不是参照、而是复现」的证据。
//!
//! `app/tests/fixtures/golden_*.json` 由参考实现（`obsidian-graph-lab.source.html`
//! 的 GraphPhysics 工厂）在 Node 里跑出来：同一张图、同一参数、`Math.random` 换成
//! 与 Rust 共用的 LCG（seed ^ 0x9e3779b9），跑 N 步之后记下每个节点的坐标。
//! Rust 侧做同样的事，坐标必须一致（世界单位 ε ≤ 1e-6）。

use std::path::PathBuf;

use serde_json::Value as Json;
use xirang_app::obsidian::physics::Physics;
use xirang_core::codec::Uuid;

fn fixture(name: &str) -> Json {
    let path: PathBuf = [env!("CARGO_MANIFEST_DIR"), "tests", "fixtures", name]
        .iter()
        .collect();
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("读不到 {}: {e}", path.display()));
    serde_json::from_str(&text).expect("fixture 应当是合法 JSON")
}

/// 按 fixture 建物理世界并推进，返回 (id, x, y) 列表。
fn run(fx: &Json) -> Vec<(String, f64, f64)> {
    let seed = fx["seed"].as_u64().unwrap() as u32;
    let steps = fx["steps"].as_u64().unwrap() as usize;

    let nodes = fx["nodes"].as_array().unwrap();
    let ids: Vec<Uuid> = nodes
        .iter()
        .map(|n| Uuid::parse(n["id"].as_str().unwrap()).unwrap())
        .collect();
    let weights = vec![0usize; ids.len()];
    let positions: Vec<(f64, f64)> = nodes
        .iter()
        .map(|n| (n["x"].as_f64().unwrap(), n["y"].as_f64().unwrap()))
        .collect();
    let index_of: std::collections::HashMap<&str, usize> = nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (n["id"].as_str().unwrap(), i))
        .collect();

    let mut p = Physics::new();
    // 力参数：fixture 里给的是**引擎值**（不是滑杆值），直接落到字段上
    p.center_strength = fx["forces"]["centerStrength"].as_f64().unwrap();
    p.link_strength = fx["forces"]["linkStrength"].as_f64().unwrap();
    p.link_distance = fx["forces"]["linkDistance"].as_f64().unwrap();
    p.set_repel(fx["forces"]["repelStrength"].as_f64().unwrap());
    // jiggle 的随机流：与 JS 侧同一支 LCG、同一 seed
    p.rng = xirang_app::obsidian::physics::Lcg::new(seed ^ 0x9e3779b9);
    p.set_nodes(&ids, &weights);
    p.set_positions(&positions);
    let pairs: Vec<(usize, usize)> = fx["links"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| {
            let a = l[0].as_str().unwrap();
            let b = l[1].as_str().unwrap();
            (index_of[a], index_of[b])
        })
        .collect();
    p.set_links(&pairs);
    p.recompute_link_weights();
    p.alpha = 1.0;
    p.alpha_target = 0.0;

    for _ in 0..steps {
        p.alpha += (0.0 - p.alpha) * xirang_app::obsidian::physics::alpha_decay();
        p.step();
    }
    p.ids
        .iter()
        .enumerate()
        .map(|(i, id)| (id.to_string(), p.nodes[i].x, p.nodes[i].y))
        .collect()
}

fn compare(name: &str) {
    let fx = fixture(name);
    let got = run(&fx);
    let expected = fx["expected"].as_array().unwrap();
    assert_eq!(got.len(), expected.len());
    let mut worst: f64 = 0.0;
    let mut worst_id = String::new();
    for (i, e) in expected.iter().enumerate() {
        let id = e["id"].as_str().unwrap();
        assert_eq!(got[i].0, id, "节点顺序必须一致");
        let dx = got[i].1 - e["x"].as_f64().unwrap();
        let dy = got[i].2 - e["y"].as_f64().unwrap();
        let d = (dx * dx + dy * dy).sqrt();
        if d > worst {
            worst = d;
            worst_id = id.to_string();
        }
    }
    println!("{name}: {} 个节点，最大偏差 {worst:.3e}（节点 {worst_id}）", got.len());
    assert!(
        worst <= 1e-6,
        "{name} 与 JS 参考不一致：最大偏差 {worst:.3e}（节点 {worst_id}）"
    );
}

#[test]
fn matches_js_reference_small() {
    compare("golden_small.json");
}

#[test]
fn matches_js_reference_big() {
    compare("golden_big.json");
}
