//! 息壤索引对比基准：工作区台账（wsidx） vs 每文件侧车（XRIDX） vs 整份载入。
//!
//! 用法：
//!   xr-bench gen  --root <目录> [--kind multi|single|shards] [--nodes 1e5] [--seed N]
//!   xr-bench run  --out <目录> [--nodes 4,5,6] [--bin-dir <release 目录>] [--reps 3]
//!   xr-bench report --out <目录>
//!
//! `run` 会：造夹具 → 建三种索引 → 跑场景矩阵 → 打印表格 → 写 raw/*.json 与 report.md。

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

use serde_json::{json, Value};
use xirang_core::codec::Uuid;
use xirang_core::{fixture, index, wsidx};

// ---------------------------------------------------------------- 小工具

fn median(mut v: Vec<f64>) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn dir_size(p: &Path) -> u64 {
    let mut total = 0;
    if let Ok(rd) = std::fs::read_dir(p) {
        for e in rd.flatten() {
            let path = e.path();
            if path.is_dir() {
                total += dir_size(&path);
            } else {
                total += e.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
    }
    total
}

fn rss_bytes(who: i32) -> u64 {
    unsafe {
        let mut u: libc::rusage = std::mem::zeroed();
        libc::getrusage(who, &mut u);
        let v = u.ru_maxrss as u64;
        if cfg!(target_os = "macos") {
            v
        } else {
            v * 1024
        }
    }
}

fn self_rss_mb() -> f64 {
    rss_bytes(libc::RUSAGE_SELF) as f64 / 1e6
}

fn children_rss_mb() -> f64 {
    rss_bytes(libc::RUSAGE_CHILDREN) as f64 / 1e6
}

fn parse_numeric(s: &str) -> usize {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix("1e") {
        return 10usize.pow(rest.parse::<u32>().unwrap_or(4));
    }
    if let Some(rest) = s.strip_prefix("1E") {
        return 10usize.pow(rest.parse::<u32>().unwrap_or(4));
    }
    s.parse::<usize>().unwrap_or(10_000)
}

/// 目标节点数 → 夹具规格（每条词条约 9 个节点）。
fn spec_for_nodes(target: usize, kind: fixture::FixtureKind, name: &str, seed: u64) -> fixture::FixtureSpec {
    let entries = (target / 9).max(1);
    let entries_per_file = 250;
    let files = ((entries + entries_per_file - 1) / entries_per_file).max(4);
    fixture::FixtureSpec {
        kind,
        files,
        entries_per_file: (entries / files).max(1),
        inherit_ratio: 1,
        seed,
        name: name.to_string(),
    }
}

// ---------------------------------------------------------------- 单模式测量

#[derive(Clone, Debug, Default)]
struct ModeResult {
    mode: String,
    build_ms: f64,
    index_bytes: u64,
    locate_us: f64,
    children_us: f64,
    references_us: f64,
    subtree_us: f64,
    append_ms: f64,
    append_bytes: u64,
    compact_ms: f64,
    peak_rss_mb: f64,
    opens_per_query: f64,
}

/// 对一批编号做「每次单独计时 + 取中位数」的查询测量。
fn query_times<F>(b: &mut dyn index::Backend, ids: &[Uuid], mut f: F) -> (f64, f64)
where
    F: FnMut(&mut dyn index::Backend, Uuid),
{
    let n = ids.len().min(200).max(1);
    let mut ts = Vec::with_capacity(n);
    let opens_before = b.opens();
    for id in ids.iter().take(n) {
        let t = Instant::now();
        f(b, *id);
        ts.push(t.elapsed().as_secs_f64() * 1e6);
    }
    let opens = (b.opens().saturating_sub(opens_before)) as f64 / n as f64;
    (median(ts), opens)
}

fn bench_mode(
    mode: &str,
    root: &Path,
    info: &fixture::FixtureInfo,
    reps: usize,
) -> ModeResult {
    let paths: Vec<String> = info.files.iter().map(|p| p.to_string_lossy().into_owned()).collect();
    let mut r = ModeResult { mode: mode.to_string(), ..Default::default() };
    let idx_dir = wsidx::index_dir(root);
    let _ = std::fs::remove_dir_all(&idx_dir);
    if mode == "sidecar" {
        for p in &info.files {
            let _ = std::fs::remove_file(index::sidecar_path(p));
        }
    }

    // 建索引
    let t = Instant::now();
    match mode {
        "workspace" => {
            wsidx::rebuild(root, &[]).unwrap();
            r.index_bytes = dir_size(&idx_dir);
        }
        "sidecar" => {
            for p in &info.files {
                index::rebuild(p).unwrap();
            }
            r.index_bytes =
                info.files.iter().map(|p| index::sidecar_path(p).metadata().map(|m| m.len()).unwrap_or(0)).sum();
        }
        _ => {
            for p in &info.files {
                let _ = xirang_core::tree::Store::load(p).unwrap();
            }
            r.index_bytes = 0;
        }
    }
    r.build_ms = t.elapsed().as_secs_f64() * 1000.0;

    // 查询场景
    let word_ids = info.word_ids.clone();
    let entry_ids = info.entry_ids.clone();
    let inherit_ids = info.inherit_ids.clone();
    let referenced = info.referenced_ids.clone();
    {
        let mut b: Box<dyn index::Backend> = match mode {
            "workspace" => Box::new(index::WorkspaceBackend::open(root).unwrap()),
            "sidecar" => Box::new(index::SidecarBackend::open(&paths).unwrap()),
            _ => Box::new(index::MemoryBackend::load(&paths).unwrap()),
        };
        let (us, opens) = query_times(b.as_mut(), &word_ids, |b, id| {
            let _ = b.locate(id);
        });
        r.locate_us = us;
        r.opens_per_query = opens;
        let (us, _) = query_times(b.as_mut(), &entry_ids, |b, id| {
            let _ = b.children(id);
        });
        r.children_us = us;
        let (us, _) = query_times(b.as_mut(), &referenced, |b, id| {
            let _ = b.references(id);
        });
        r.references_us = us;
        let (us, _) = query_times(b.as_mut(), &inherit_ids, |b, id| {
            let _ = b.subtree(id);
        });
        r.subtree_us = us;
    }

    // 追加写：改一个词形的值，然后维护索引
    if mode != "memory" {
        let target = info.word_ids[0];
        let mut store = xirang_core::tree::Store::load(&info.files[0]).unwrap();
        store
            .update(target, xirang_core::codec::Value::Text("基准改动".into()))
            .unwrap();
        store.save(&info.files[0]).unwrap();
        let t = Instant::now();
        match mode {
            "workspace" => {
                let before = dir_size(&idx_dir);
                wsidx::append_file(root, &info.files[0]).unwrap();
                r.append_bytes = dir_size(&idx_dir).saturating_sub(before);
            }
            _ => {
                // 侧车每次保存都整份重建 → 写放大 = 重建后的整份大小
                index::rebuild(&info.files[0]).unwrap();
                r.append_bytes =
                    index::sidecar_path(&info.files[0]).metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
        r.append_ms = t.elapsed().as_secs_f64() * 1000.0;
    }

    // 压实（仅工作区台账）
    if mode == "workspace" {
        let mut ts = Vec::new();
        for _ in 0..reps.max(1) {
            let t = Instant::now();
            wsidx::compact(root).unwrap();
            ts.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        r.compact_ms = median(ts);
    }
    r.peak_rss_mb = self_rss_mb();
    r
}

// ---------------------------------------------------------------- 入口测量（CLI / MCP）

#[derive(Clone, Debug, Default)]
struct EntryResult {
    name: String,
    ms: f64,
    // 说明：子进程峰值是「历史最大」，只作参考
    child_peak_mb: f64,
    ok: bool,
    detail: String,
}

fn run_cmd(mut c: Command) -> (bool, String) {
    match c.output() {
        Ok(o) => (
            o.status.success(),
            String::from_utf8_lossy(&o.stderr).lines().next().unwrap_or("").to_string(),
        ),
        Err(e) => (false, e.to_string()),
    }
}

fn bench_entrypoints(
    root: &Path,
    info: &fixture::FixtureInfo,
    bin_dir: &Path,
    index_mode: &str,
    reps: usize,
) -> Vec<EntryResult> {
    let mut out = Vec::new();
    // 绝对路径：CLI 会在 root 目录下运行，相对路径会失效
    let bin_dir = bin_dir.canonicalize().unwrap_or_else(|_| bin_dir.to_path_buf());
    let xr = bin_dir.join("xr");
    let mcp = bin_dir.join("xr-mcp");
    if !xr.exists() || !mcp.exists() {
        out.push(EntryResult {
            name: "cli/mcp".into(),
            ok: false,
            detail: format!("找不到 release 产物：{}（先 cargo build --release）", bin_dir.display()),
            ..Default::default()
        });
        return out;
    }
    let target = info.word_ids[0].to_string();
    let files: Vec<String> = info
        .files
        .iter()
        .take(200)
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();

    // CLI：ws（走索引的跨文件查询）
    {
        let mut ts = Vec::new();
        let mut ok = true;
        let mut detail = String::new();
        for i in 0..reps {
            let mut c = Command::new(&xr);
            c.current_dir(root)
                .env("XIRANG_INDEX_MODE", index_mode)
                .env("XIRANG_INDEX_MAINTENANCE", "off") // 基准里不让后台压实干扰
                .env("XIRANG_CATALOG", root.join("bench-catalog.idx"))
                .arg("ws")
                .arg(&target);
            for f in &files {
                c.arg(f);
            }
            let t = Instant::now();
            let (good, err) = run_cmd(c);
            ts.push(t.elapsed().as_secs_f64() * 1000.0);
            if i == 0 {
                ok = good;
                detail = err;
            }
        }
        out.push(EntryResult {
            name: format!("cli ws（{index_mode}）"),
            ms: median(ts),
            child_peak_mb: children_rss_mb(),
            ok,
            detail,
        });
    }

    // CLI：refs（整份载入路径）
    {
        let mut ts = Vec::new();
        let mut ok = true;
        for i in 0..reps {
            let mut c = Command::new(&xr);
            c.current_dir(root)
                .env("XIRANG_INDEX_MODE", index_mode)
                .env("XIRANG_INDEX_MAINTENANCE", "off")
                .env("XIRANG_CATALOG", root.join("bench-catalog.idx"))
                .arg("refs")
                .arg(files.first().cloned().unwrap_or_default())
                .arg(&target);
            let t = Instant::now();
            let (good, _) = run_cmd(c);
            ts.push(t.elapsed().as_secs_f64() * 1000.0);
            if i == 0 {
                ok = good;
            }
        }
        out.push(EntryResult { name: "cli refs（整份载入）".into(), ms: median(ts), child_peak_mb: children_rss_mb(), ok, detail: String::new() });
    }

    // MCP：常驻会话里的 query(refs)
    {
        let mut child = match Command::new(&mcp)
            .arg("--root")
            .arg(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                out.push(EntryResult { name: "mcp query(refs)".into(), ok: false, detail: e.to_string(), ..Default::default() });
                return out;
            }
        };
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let mut ts = Vec::new();
        let mut ok = true;
        let mut detail = String::new();
        for i in 0..reps {
            let req = json!({
                "jsonrpc": "2.0", "id": i + 1, "method": "tools/call",
                "params": {"name": "query", "arguments": {
                    "file": files.first().cloned().unwrap_or_default(),
                    "action": "refs", "node": target
                }}
            });
            let t = Instant::now();
            writeln!(stdin, "{req}").ok();
            stdin.flush().ok();
            let mut line = String::new();
            if stdout.read_line(&mut line).is_err() || line.is_empty() {
                ok = false;
                detail = "MCP 无响应".into();
                break;
            }
            ts.push(t.elapsed().as_secs_f64() * 1000.0);
            if i == 0 {
                let v: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
                ok = !v["result"]["isError"].as_bool().unwrap_or(false);
                if !ok {
                    detail = v["result"]["content"][0]["text"].to_string();
                }
            }
        }
        let _ = child.kill();
        let _ = child.wait();
        out.push(EntryResult { name: "mcp query(refs)（常驻会话）".into(), ms: median(ts), child_peak_mb: children_rss_mb(), ok, detail });
    }
    out
}

// ---------------------------------------------------------------- run / report

#[derive(Clone, Debug, Default)]
struct ScaleResult {
    target_nodes: usize,
    nodes: usize,
    files: usize,
    kind: String,
    data_bytes: u64,
    modes: Vec<ModeResult>,
    entries: Vec<EntryResult>,
}

fn run_scale(
    out_dir: &Path,
    target: usize,
    kind: fixture::FixtureKind,
    bin_dir: &Path,
    reps: usize,
    clean: bool,
    keep: bool,
) -> ScaleResult {
    let kind_name = match kind {
        fixture::FixtureKind::Multi => "multi",
        fixture::FixtureKind::Single => "single",
        fixture::FixtureKind::Shards => "shards",
    };
    let root = out_dir.join(format!("data-{kind_name}-{target}"));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let spec = spec_for_nodes(target, kind, "cibase", 20260925);
    eprintln!("[scale {target}] 造夹具：{} 个文件 × {} 条词条", spec.files, spec.entries_per_file);
    let info = fixture::build(&root, &spec).unwrap();
    let data_bytes: u64 = info.files.iter().map(|p| p.metadata().map(|m| m.len()).unwrap_or(0)).sum();
    eprintln!("[scale {target}] 实际 {} 个节点 · 数据 {:.2} MB", info.nodes, data_bytes as f64 / 1e6);

    let mut modes = Vec::new();
    for mode in ["workspace", "sidecar", "memory"] {
        eprintln!("[scale {target}] 测 {mode}");
        modes.push(bench_mode(mode, &root, &info, reps));
    }

    // 入口：两种索引模式各跑一遍 CLI/MCP
    let mut entries = Vec::new();
    for m in ["workspace", "sidecar"] {
        // 跑之前确保该模式的索引是刚建好的
        bench_mode(m, &root, &info, 1);
        entries.extend(bench_entrypoints(&root, &info, bin_dir, m, reps.max(3)));
    }

    let result = ScaleResult {
        target_nodes: target,
        nodes: info.nodes,
        files: info.files.len(),
        kind: kind_name.to_string(),
        data_bytes,
        modes,
        entries,
    };
    if clean && !keep {
        let _ = std::fs::remove_dir_all(&root);
    }
    result
}

fn print_scale(s: &ScaleResult) {
    println!(
        "\n=== 规模 {}（实际 {} 节点 · {} 个文件 · 数据 {:.2} MB）===",
        s.target_nodes,
        s.nodes,
        s.files,
        s.data_bytes as f64 / 1e6
    );
    println!(
        "{:<12} {:>10} {:>12} {:>10} {:>10} {:>10} {:>10} {:>12} {:>10}",
        "模式", "建索引ms", "索引MB", "点查µs", "孩子µs", "反向µs", "整树µs", "追加ms", "压实ms"
    );
    for m in &s.modes {
        println!(
            "{:<12} {:>10.1} {:>12.2} {:>10.1} {:>10.1} {:>10.1} {:>10.1} {:>10.1} {:>10.1}",
            m.mode,
            m.build_ms,
            m.index_bytes as f64 / 1e6,
            m.locate_us,
            m.children_us,
            m.references_us,
            m.subtree_us,
            m.append_ms,
            m.compact_ms
        );
    }
    for e in &s.entries {
        println!(
            "  {:<28} {:>8.1} ms   子进程峰值 {:>6.1} MB   {}",
            e.name,
            e.ms,
            e.child_peak_mb,
            if e.ok { "ok".to_string() } else { format!("失败：{}", e.detail) }
        );
    }
}

fn write_report(out_dir: &Path, scales: &[ScaleResult]) -> std::io::Result<()> {
    let mut md = String::new();
    md.push_str("# 息壤索引对比报告（工作区台账 vs 每文件侧车 vs 整份载入）\n\n");
    md.push_str("生成时间：`");
    md.push_str(&chrono_stamp());
    md.push_str("`；夹具为词典型（词条树 + 继承树 + 跨文件引用），编号由固定种子生成。\n\n");
    md.push_str("单位：耗时 ms（建索引 / 追加 / 压实）与 µs（每次查询的中位数）；索引体积 MB；内存为进程峰值。\n\n");
    md.push_str("索引体积说明：三本台账之和实测约为数据体积的 2.4 倍（定位 52 B/节点 + 关系 68 B/边 + 反向 36 B/引用边）。\n\n");
    for s in scales {
        md.push_str(&format!(
            "## {} 节点（实际 {} · {} 个文件 · 数据 {:.2} MB）\n\n",
            s.target_nodes,
            s.nodes,
            s.files,
            s.data_bytes as f64 / 1e6
        ));
        md.push_str("| 模式 | 建索引 ms | 索引 MB | 点查 µs | 孩子 µs | 反向 µs | 整树 µs | 追加 ms | 追加字节 | 压实 ms | 峰值内存 MB | 每次查询打开文件数 |\n");
        md.push_str("|---|---|---|---|---|---|---|---|---|---|---|---|\n");
        for m in &s.modes {
            md.push_str(&format!(
                "| {} | {:.1} | {:.2} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} | {} | {:.1} | {:.0} | {:.1} |\n",
                m.mode, m.build_ms, m.index_bytes as f64 / 1e6, m.locate_us, m.children_us,
                m.references_us, m.subtree_us, m.append_ms, m.append_bytes, m.compact_ms,
                m.peak_rss_mb, m.opens_per_query
            ));
        }
        md.push_str("\n**入口对比（同一查询走 CLI / MCP）**\n\n");
        md.push_str("| 入口 | 耗时 ms | 子进程峰值 MB | 结果 |\n|---|---|---|---|\n");
        for e in &s.entries {
            md.push_str(&format!(
                "| {} | {:.1} | {:.1} | {} |\n",
                e.name,
                e.ms,
                e.child_peak_mb,
                if e.ok { "ok" } else { "失败" }
            ));
        }
        md.push('\n');
    }
    std::fs::write(out_dir.join("report.md"), md)?;
    Ok(())
}

fn chrono_stamp() -> String {
    // 不引依赖：用系统时间近似 ISO 时间戳（秒级）
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("unix:{secs}")
}

fn main() {
    // 基准自身不触发后台整理（避免测量被干扰）
    std::env::set_var("XIRANG_INDEX_MAINTENANCE", "off");
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(|s| s.as_str()).unwrap_or("run");
    let mut opts: BTreeMap<String, String> = BTreeMap::new();
    let mut i = 1;
    while i < args.len() {
        if let Some(k) = args[i].strip_prefix("--") {
            let v = args.get(i + 1).cloned().unwrap_or_default();
            opts.insert(k.to_string(), v);
            i += 2;
        } else {
            i += 1;
        }
    }
    let out_dir = PathBuf::from(opts.get("out").cloned().unwrap_or_else(|| ".exp/index-compare".into()));
    let bin_dir = PathBuf::from(opts.get("bin-dir").cloned().unwrap_or_else(|| "rust/target/release".into()));
    let reps: usize = opts.get("reps").and_then(|s| s.parse().ok()).unwrap_or(3);

    match cmd {
        "gen" => {
            let root = PathBuf::from(opts.get("root").cloned().unwrap_or_else(|| "gen".into()));
            let target = opts.get("nodes").map(|s| parse_numeric(s)).unwrap_or(100_000);
            let kind = match opts.get("kind").map(|s| s.as_str()).unwrap_or("multi") {
                "single" => fixture::FixtureKind::Single,
                "shards" => fixture::FixtureKind::Shards,
                _ => fixture::FixtureKind::Multi,
            };
            let seed = opts.get("seed").and_then(|s| s.parse().ok()).unwrap_or(20260925);
            let spec = spec_for_nodes(target, kind, "cibase", seed);
            let info = fixture::build(&root, &spec).unwrap();
            println!(
                "已生成：{} 个节点 · {} 个文件 · 根 {}",
                info.nodes,
                info.files.len(),
                root.display()
            );
        }
        "report" => {
            let raw_dir = out_dir.join("raw");
            let mut scales: Vec<ScaleResult> = Vec::new();
            if let Ok(rd) = std::fs::read_dir(&raw_dir) {
                for e in rd.flatten() {
                    if let Ok(txt) = std::fs::read_to_string(e.path()) {
                        if let Ok(v) = serde_json::from_str::<Value>(&txt) {
                            scales.push(from_json(&v));
                        }
                    }
                }
            }
            scales.sort_by_key(|s| s.target_nodes);
            write_report(&out_dir, &scales).unwrap();
            println!("已写出 {}", out_dir.join("report.md").display());
        }
        _ => {
            std::fs::create_dir_all(out_dir.join("raw")).unwrap();
            let targets: Vec<usize> = opts
                .get("nodes")
                .map(|s| s.split(',').map(parse_numeric).collect())
                .unwrap_or_else(|| vec![10_000, 100_000, 1_000_000]);
            let kind = match opts.get("kind").map(|s| s.as_str()).unwrap_or("multi") {
                "single" => fixture::FixtureKind::Single,
                "shards" => fixture::FixtureKind::Shards,
                _ => fixture::FixtureKind::Multi,
            };
            let mut scales = Vec::new();
            let clean = opts.contains_key("clean");
            let keep = opts.contains_key("keep");
            for t in targets {
                let s = run_scale(&out_dir, t, kind, &bin_dir, reps, clean, keep);
                print_scale(&s);
                std::fs::write(out_dir.join("raw").join(format!("{t}.json")), to_json(&s)).unwrap();
                scales.push(s);
            }
            write_report(&out_dir, &scales).unwrap();
            println!("\n报告：{}", out_dir.join("report.md").display());
            if opts.contains_key("assert") {
                std::process::exit(check_assertions(&scales));
            }
        }
    }
}

/// 与机器无关的判据：随规模增长不失控、索引体积不失控、入口都能跑通。
fn check_assertions(scales: &[ScaleResult]) -> i32 {
    let mut bad = Vec::new();
    if let (Some(first), Some(last)) = (scales.first(), scales.last()) {
        let a = first.modes.iter().find(|m| m.mode == "workspace");
        let b = last.modes.iter().find(|m| m.mode == "workspace");
        if let (Some(a), Some(b)) = (a, b) {
            for (name, x, y) in [
                ("点查", a.locate_us, b.locate_us),
                ("孩子", a.children_us, b.children_us),
                ("反向", a.references_us, b.references_us),
                ("整树", a.subtree_us, b.subtree_us),
            ] {
                // 规模涨 10×（或更多）时，每次查询耗时不得涨到 3 倍以上
                if x > 0.0 && y > x * 3.0 {
                    bad.push(format!("{name} 耗时随规模涨到 {:.1}×（{x:.1}µs → {y:.1}µs）", y / x));
                }
            }
            // 三本台账之和实测约为数据的 2.4 倍，这里给 3 倍的上限
            if last.data_bytes > 0 && b.index_bytes > last.data_bytes * 3 {
                bad.push(format!(
                    "索引体积 {:.1}× 数据（{} 字节 > {} 字节的 3 倍）",
                    b.index_bytes as f64 / last.data_bytes as f64,
                    b.index_bytes,
                    last.data_bytes
                ));
            }
        }
    }
    for s in scales {
        for e in &s.entries {
            if !e.ok {
                bad.push(format!("入口失败：{}（{}）", e.name, e.detail));
            }
        }
    }
    if bad.is_empty() {
        println!("断言通过：查询耗时随规模稳定、索引体积受控、CLI / MCP 入口都通");
        0
    } else {
        eprintln!("断言失败：{}", bad.join("；"));
        1
    }
}

// ---------------------------------------------------------------- 序列化

fn to_json(s: &ScaleResult) -> String {
    let modes: Vec<Value> = s
        .modes
        .iter()
        .map(|m| {
            json!({
                "mode": m.mode, "build_ms": m.build_ms, "index_bytes": m.index_bytes,
                "locate_us": m.locate_us, "children_us": m.children_us,
                "references_us": m.references_us, "subtree_us": m.subtree_us,
                "append_ms": m.append_ms, "append_bytes": m.append_bytes,
                "compact_ms": m.compact_ms, "peak_rss_mb": m.peak_rss_mb,
                "opens_per_query": m.opens_per_query,
            })
        })
        .collect();
    let entries: Vec<Value> = s
        .entries
        .iter()
        .map(|e| json!({"name": e.name, "ms": e.ms, "child_peak_mb": e.child_peak_mb, "ok": e.ok, "detail": e.detail}))
        .collect();
    json!({
        "target_nodes": s.target_nodes, "nodes": s.nodes, "files": s.files,
        "kind": s.kind, "data_bytes": s.data_bytes, "modes": modes, "entries": entries,
    })
    .to_string()
}

fn from_json(v: &Value) -> ScaleResult {
    let modes = v["modes"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|m| ModeResult {
            mode: m["mode"].as_str().unwrap_or("").to_string(),
            build_ms: m["build_ms"].as_f64().unwrap_or(0.0),
            index_bytes: m["index_bytes"].as_u64().unwrap_or(0),
            locate_us: m["locate_us"].as_f64().unwrap_or(0.0),
            children_us: m["children_us"].as_f64().unwrap_or(0.0),
            references_us: m["references_us"].as_f64().unwrap_or(0.0),
            subtree_us: m["subtree_us"].as_f64().unwrap_or(0.0),
            append_ms: m["append_ms"].as_f64().unwrap_or(0.0),
            append_bytes: m["append_bytes"].as_u64().unwrap_or(0),
            compact_ms: m["compact_ms"].as_f64().unwrap_or(0.0),
            peak_rss_mb: m["peak_rss_mb"].as_f64().unwrap_or(0.0),
            opens_per_query: m["opens_per_query"].as_f64().unwrap_or(0.0),
        })
        .collect();
    let entries = v["entries"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|e| EntryResult {
            name: e["name"].as_str().unwrap_or("").to_string(),
            ms: e["ms"].as_f64().unwrap_or(0.0),
            child_peak_mb: e["child_peak_mb"].as_f64().unwrap_or(0.0),
            ok: e["ok"].as_bool().unwrap_or(false),
            detail: e["detail"].as_str().unwrap_or("").to_string(),
        })
        .collect();
    ScaleResult {
        target_nodes: v["target_nodes"].as_u64().unwrap_or(0) as usize,
        nodes: v["nodes"].as_u64().unwrap_or(0) as usize,
        files: v["files"].as_u64().unwrap_or(0) as usize,
        kind: v["kind"].as_str().unwrap_or("").to_string(),
        data_bytes: v["data_bytes"].as_u64().unwrap_or(0),
        modes,
        entries,
    }
}
