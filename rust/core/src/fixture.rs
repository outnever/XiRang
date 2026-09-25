//! 测试夹具：生成词典型的工作区（一个文件里多棵树 + 跨文件引用 + 可选分片）。
//!
//! 用途：单元测试的对拍、以及 `rust/bench` 的对比基准。**完全确定性**——
//! 编号由固定种子的 splitmix64 生成，所以同一 spec 每次跑出来的节点编号一模一样。

use std::fs;
use std::path::{Path, PathBuf};

use crate::codec::{Node, Uuid, Value};
use crate::tree::Store;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FixtureKind {
    /// 一个文件（里面多个部类树）
    Single,
    /// 多个文件（每个文件一个部类树），文件之间互相引用
    Multi,
    /// 分片词库目录（一个大文件按顶层根拆成分片 + 清单）
    Shards,
}

#[derive(Clone, Debug)]
pub struct FixtureSpec {
    pub kind: FixtureKind,
    /// 文件数（Single 时为顶层根数；Shards 时为分片数）
    pub files: usize,
    /// 每个文件（部类）里的词条数
    pub entries_per_file: usize,
    /// 平均每条词条挂多少个「继承树」（引用别的文件里的词条）
    pub inherit_ratio: usize,
    /// 随机种子
    pub seed: u64,
    /// 文件名前缀
    pub name: String,
}

impl Default for FixtureSpec {
    fn default() -> Self {
        FixtureSpec {
            kind: FixtureKind::Multi,
            files: 4,
            entries_per_file: 250,
            inherit_ratio: 1,
            seed: 20260925,
            name: "cibase".into(),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct FixtureInfo {
    /// 工作区根
    pub root: PathBuf,
    /// 数据文件（分片模式下是各分片文件 + 清单）
    pub files: Vec<PathBuf>,
    /// 抽样用的词条根编号
    pub entry_ids: Vec<Uuid>,
    /// 抽样用的「词形」子节点编号
    pub word_ids: Vec<Uuid>,
    /// 抽样用的继承树根编号
    pub inherit_ids: Vec<Uuid>,
    /// 被引用（跨文件）的编号
    pub referenced_ids: Vec<Uuid>,
    /// 实际节点总数
    pub nodes: usize,
}

/// splitmix64：小而稳定的确定性伪随机源。
struct Prng(u64);

impl Prng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn uuid(&mut self) -> Uuid {
        let mut b = [0u8; 16];
        for i in 0..2 {
            let v = self.next().to_be_bytes();
            b[i * 8..(i + 1) * 8].copy_from_slice(&v);
        }
        // 变体 / 版本位与 v4 对齐（只是为了让编号看起来正常）
        b[6] = (b[6] & 0x0f) | 0x40;
        b[8] = (b[8] & 0x3f) | 0x80;
        Uuid(b)
    }

}

fn node(prng: &mut Prng, parent: Option<Uuid>, name: &str, value: Value) -> Node {
    Node { id: prng.uuid(), parent, name: name.to_string(), value }
}

/// 造一个有内容的词条树：词条 → 词形 / 拼音 / 释义 → 义项 [+ @来源]。
fn build_entry(store: &mut Store, prng: &mut Prng, parent: Uuid, idx: usize) -> (Uuid, Uuid) {
    let e = node(prng, Some(parent), "词条", Value::Empty);
    let entry_id = e.id;
    store.add(e);
    let w = node(prng, Some(entry_id), "词形", Value::Text(format!("词{idx}")));
    let word_id = w.id;
    store.add(w);
    store.add(node(prng, Some(entry_id), "拼音", Value::Text(format!("ci{idx}"))));
    let shi = node(prng, Some(entry_id), "释义", Value::Empty);
    let shi_id = shi.id;
    store.add(shi);
    store.add(node(
        prng,
        Some(shi_id),
        "义项",
        Value::Text(format!("第 {idx} 条的释义")),
    ));
    store.add(node(prng, Some(shi_id), "@来源", Value::Text("CiBase".into())));
    (entry_id, word_id)
}

/// 造一棵继承树：继承 → 基引用（跨文件指向别的词条）+ 覆盖字段。
fn build_inherit(
    store: &mut Store,
    prng: &mut Prng,
    parent: Uuid,
    target: Uuid,
    idx: usize,
) -> Uuid {
    let t = node(prng, Some(parent), "继承", Value::Empty);
    let id = t.id;
    store.add(t);
    store.add(node(prng, Some(id), "基引用", Value::Reference(target)));
    store.add(node(prng, Some(id), "覆盖", Value::Text(format!("覆盖{idx}"))));
    id
}

/// 生成夹具；返回文件清单与抽样编号。
pub fn build(dir: &Path, spec: &FixtureSpec) -> Result<FixtureInfo, String> {
    fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let mut prng = Prng(spec.seed);
    let mut info = FixtureInfo { root: dir.to_path_buf(), ..Default::default() };
    let mut per_file_entries: Vec<Vec<(Uuid, Uuid)>> = Vec::new();

    // 第一遍：先造出各文件的词条，记下编号（跨文件引用要指向它们）
    for f in 0..spec.files {
        let mut store = Store::new();
        let root = node(&mut prng, None, &format!("部类{f}"), Value::Empty);
        let root_id = root.id;
        store.add(root);
        let mut entries = Vec::new();
        for i in 0..spec.entries_per_file {
            entries.push(build_entry(&mut store, &mut prng, root_id, f * spec.entries_per_file + i));
        }
        per_file_entries.push(entries);
        let path = dir.join(format!("{}-{f:03}.xirang", spec.name));
        store.save(&path).map_err(|e| e.to_string())?;
        info.nodes += store.len();
        info.files.push(path);
    }

    // 第二遍：给每个文件补上继承树（指向别的文件里的词条），再存一次
    for f in 0..spec.files {
        let mut store = Store::load(&info.files[f]).map_err(|e| e.to_string())?;
        let root_id = store
            .roots()
            .first()
            .map(|n| n.id)
            .ok_or_else(|| "夹具文件没有根".to_string())?;
        let other = (f + 1) % spec.files.max(1);
        let targets = &per_file_entries[other];
        let n_inherit = spec.entries_per_file * spec.inherit_ratio;
        for k in 0..n_inherit {
            if targets.is_empty() {
                break;
            }
            let target = targets[k % targets.len()].0;
            let id = build_inherit(&mut store, &mut prng, root_id, target, k);
            info.inherit_ids.push(id);
            info.referenced_ids.push(target);
        }
        store.save(&info.files[f]).map_err(|e| e.to_string())?;
        info.entry_ids.extend(per_file_entries[f].iter().map(|(e, _)| *e));
        info.word_ids.extend(per_file_entries[f].iter().map(|(_, w)| *w));
    }
    info.nodes = info
        .files
        .iter()
        .map(|p| Store::load(p).map(|s| s.len()).unwrap_or(0))
        .sum();

    // 分片模式：把多个文件合成一个再按顶层根拆开
    if spec.kind == FixtureKind::Shards {
        let mut merged = Store::new();
        for p in &info.files {
            let s = Store::load(p).map_err(|e| e.to_string())?;
            for n in s.nodes() {
                merged.add(n.clone());
            }
        }
        for p in &info.files {
            let _ = fs::remove_file(p);
        }
        let shard_dir = dir.join(format!("{}.shards", spec.name));
        let rule = crate::shard::ShardRule::parse("root")?;
        crate::shard::split_to_dir(&merged, &rule, &shard_dir).map_err(|e| e.to_string())?;
        info.files = crate::wsidx::collect_data_files(&shard_dir);
        info.nodes = merged.len();
    }
    Ok(info)
}
