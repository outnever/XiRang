//! 侧车索引（实现层缓存）：只索引「树根」+「节点→所属根」映射 + 反向引用边，
//! 供跨文件按树懒加载。不进入内核 / 协议规范，也不改动 `.xirang` 本体。
//!
//! sidecar 文件 = `<文件>.xirang.idx`，二进制（大端）。三个条目块都按 UUID 的
//! 16 字节升序排序，读取端用「seek + 二分查找」做 O(log N) 的随机查找，
//! 不把整张索引读进内存。布局见本文件 `HEADER`。

use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use crate::codec::{self, Node, Uuid};
use crate::tree::{self, Store};

pub const MAGIC: &[u8; 5] = b"XRIDX";
pub const VERSION: u8 = 3;
pub const HEADER: &str = "\
XiRang sidecar node index (XRIDX) v3
Maps each .xirang node to its containing tree root and byte offset (implementation-layer cache).
Layout (big-endian, all blocks sorted by their first 16-byte UUID):
  magic \"XRIDX\" (5 bytes) + version (1 byte) + header_len u32 + this header
  + source_size u64 + source_mtime_sec i64 + source_mtime_nsec u32
  + node_data_start u64 + indexed_upto u64 + prefix_guard u64
  + root_count u64 + node_count u64 + edge_count u64 + child_count u64 + rev_count u64
  + root_count x (root_uuid 16B, rel_offset u64, byte_len u64)  sorted by root_uuid
  + node_count x (node_uuid 16B, root_uuid 16B, rel_offset u64, byte_len u64)  sorted by node_uuid
  + edge_count x (target_uuid 16B, source_uuid 16B)             sorted by target_uuid then source_uuid
  + child_count x (parent_uuid 16B, child_uuid 16B, rel_offset u64, byte_len u64)  sorted by parent then child
  + rev_count x (node_uuid 16B, parent_uuid 16B, root_uuid 16B, rel_offset u64, byte_len u64, ref_target 16B)
                 appended in write order; a node's last rev entry wins (append-v1 revisions)

The first four blocks describe the file as scanned up to `indexed_upto`; later appends are
recorded by appending rev entries (no rewrite of the 4 base blocks). `prefix_guard` is a hash of
the bytes just before `indexed_upto`, used to detect that an older region was rewritten.
";

/// 侧车索引（内存态，构建 / 落盘用）：根集合 + 节点归属 + 反向边。
#[derive(Clone, Debug, Default)]
pub struct NodeIndex {
    /// 源 `.xirang` 内节点数据段的绝对起始偏移。
    pub node_data_start: u64,
    /// 源文件元数据（新鲜度判定）。
    pub source_size: u64,
    pub source_mtime_sec: i64,
    pub source_mtime_nsec: u32,
    /// 已经扫描到源文件的哪个绝对偏移（之后的字节靠 rev 块增量补齐）。
    pub indexed_upto: u64,
    /// `indexed_upto` 之前一段字节的哈希（检测「文件被改写」）。
    pub prefix_guard: u64,
    /// 根 UUID → (相对偏移, 字节长)。绝对偏移 = node_data_start + 相对偏移。
    pub roots: HashMap<Uuid, (u64, u64)>,
    /// 每个节点 UUID → 其所属根 UUID（根自身映射到自己）。
    pub assign: HashMap<Uuid, Uuid>,
    /// 引用目标 UUID → 指向它的源节点 UUID 列表。
    pub reverse: HashMap<Uuid, Vec<Uuid>>,
    /// 父 UUID → [(子 UUID, 相对偏移, 字节长)]。用于跨文件「凑孩子」时直接定位。
    pub children: HashMap<Uuid, Vec<(Uuid, u64, u64)>>,
    /// 追加修订（append-v1）：按写入顺序排列，同一编号靠后者覆盖。
    pub revs: Vec<RevEntry>,
}

/// 一条追加上去的修订记录（`append-v1`）。
#[derive(Clone, Copy, Debug)]
pub struct RevEntry {
    pub id: Uuid,
    pub parent: Option<Uuid>,
    pub root: Uuid,
    pub rel_off: u64,
    pub len: u64,
    pub ref_target: Option<Uuid>,
}

pub const REV_SIZE: u64 = 80;

/// 已扫描区间之前的「守护窗口」大小（字节）。
pub const GUARD_WINDOW: u64 = 65536;

/// sidecar 路径 = 源文件全名后追加 `.idx`（如 `foo.xirang` → `foo.xirang.idx`）。
pub fn sidecar_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".idx");
    PathBuf::from(s)
}

fn mtime(meta: &std::fs::Metadata) -> (i64, u32) {
    match meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()) {
        Some(d) => (d.as_secs() as i64, d.subsec_nanos()),
        None => (0, 0),
    }
}

/// 读取 n 字节并前移 off；越界 / 溢出返回错误（不 panic）。
fn take<'a>(data: &'a [u8], off: &mut usize, n: usize) -> Result<&'a [u8], String> {
    let end = off.checked_add(n).ok_or("F007：索引损坏")?;
    if end > data.len() {
        return Err("F004：文件截断".into());
    }
    let s = &data[*off..end];
    *off = end;
    Ok(s)
}

/// 扫描单条节点：只读头 / 名字 / 值标记，跳过 text/blob 载荷（不物化大值）。
struct Rec {
    id: Uuid,
    parent: Option<Uuid>,
    name: String,
    tag: u8,
    ref_target: Option<Uuid>,
    start: usize,
    end: usize,
}

fn scan_node(data: &[u8], off: &mut usize) -> Result<Rec, String> {
    let start = *off;
    let id = Uuid(take(data, off, 16)?.try_into().unwrap());
    let p = Uuid(take(data, off, 16)?.try_into().unwrap());
    let parent = if p.is_nil() { None } else { Some(p) };
    let nlen = *take(data, off, 1)?.first().unwrap() as usize;
    let name = String::from_utf8(take(data, off, nlen)?.to_vec())
        .map_err(|_| "节点名非法 UTF-8".to_string())?;
    let tag = *take(data, off, 1)?.first().unwrap();
    let mut ref_target = None;
    match tag {
        codec::EMPTY => {}
        codec::INT | codec::FLOAT => {
            take(data, off, 8)?;
        }
        codec::BOOL => {
            take(data, off, 1)?;
        }
        codec::TEXT => {
            let n = u32::from_be_bytes(take(data, off, 4)?.try_into().unwrap()) as usize;
            take(data, off, n)?;
        }
        codec::REFERENCE => {
            ref_target = Some(Uuid(take(data, off, 16)?.try_into().unwrap()));
        }
        codec::BLOB => {
            let n = u64::from_be_bytes(take(data, off, 8)?.try_into().unwrap()) as usize;
            take(data, off, n)?;
        }
        other => return Err(format!("E008：类型标记非法 {other}")),
    }
    Ok(Rec { id, parent, name, tag, ref_target, start, end: *off })
}

/// 从文件字节构建索引（含魔数 / 版本 / 头）。
pub fn build(data: &[u8]) -> Result<NodeIndex, String> {
    let nodes_bytes = tree::parse_file(data)?;
    let node_data_start = (data.len() - nodes_bytes.len()) as u64;

    let mut recs: Vec<Rec> = Vec::new();
    let mut off = 0usize;
    while off < nodes_bytes.len() {
        recs.push(scan_node(nodes_bytes, &mut off)?);
    }
    let n = recs.len();

    // 后写覆盖：同一编号只保留最后一条记录（append-v1 语义），索引只反映当前视图。
    let mut by_id: HashMap<Uuid, usize> = HashMap::with_capacity(n);
    for (i, r) in recs.iter().enumerate() {
        by_id.insert(r.id, i);
    }
    let is_winner = |i: usize| by_id.get(&recs[i].id) == Some(&i);

    let mut children: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut is_root = vec![false; n];
    for i in 0..n {
        if !is_winner(i) {
            continue;
        }
        let r = &recs[i];
        match r.parent {
            None => is_root[i] = true,
            Some(p) => match by_id.get(&p) {
                Some(&pi) => children[pi].push(i),
                None => is_root[i] = true, // 父节点缺失 → 视作根，保证归属完整
            },
        }
    }
    // 模板根：挂 @模板(空)；实例根：挂 @实例。
    for i in 0..n {
        if !is_winner(i) || is_root[i] {
            continue;
        }
        for &ci in &children[i] {
            let c = &recs[ci];
            if (c.name == "@模板" && c.tag == codec::EMPTY) || c.name == "@实例" {
                is_root[i] = true;
            }
        }
    }

    // 节点 → 所属根（追加序 = 父先于子，正向一遍即可）。
    let mut assign: HashMap<Uuid, Uuid> = HashMap::with_capacity(n);
    for i in 0..n {
        if !is_winner(i) {
            continue;
        }
        let root = if is_root[i] {
            recs[i].id
        } else {
            recs[i].parent.and_then(|p| assign.get(&p).copied()).unwrap_or(recs[i].id)
        };
        assign.insert(recs[i].id, root);
    }

    // 每棵子树的范围：倒序（子先于父）把后代最大结束偏移向上合并。
    let mut sub_end: Vec<usize> = recs.iter().map(|r| r.end).collect();
    for i in (0..n).rev() {
        if !is_winner(i) {
            continue;
        }
        for &ci in &children[i] {
            if sub_end[ci] > sub_end[i] {
                sub_end[i] = sub_end[ci];
            }
        }
    }

    let mut roots: HashMap<Uuid, (u64, u64)> = HashMap::new();
    for i in 0..n {
        if is_winner(i) && is_root[i] {
            roots.insert(recs[i].id, (recs[i].start as u64, (sub_end[i] - recs[i].start) as u64));
        }
    }

    let mut reverse: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
    for i in 0..n {
        if !is_winner(i) {
            continue;
        }
        if let Some(t) = recs[i].ref_target {
            let r = &recs[i];
            reverse.entry(t).or_default().push(r.id);
        }
    }
    for v in reverse.values_mut() {
        v.sort_by(|a, b| a.0.cmp(&b.0));
    }

    // 父 → 孩子（含各自在文件里的偏移与长度）：跨文件「凑孩子」时用来直接定位。
    let mut child_map: HashMap<Uuid, Vec<(Uuid, u64, u64)>> = HashMap::new();
    for i in 0..n {
        if !is_winner(i) {
            continue;
        }
        let r = &recs[i];
        if let Some(p) = r.parent {
            child_map
                .entry(p)
                .or_default()
                .push((r.id, r.start as u64, (r.end - r.start) as u64));
        }
    }
    for v in child_map.values_mut() {
        v.sort_by(|a, b| a.0 .0.cmp(&b.0 .0));
    }

    let upto = data.len() as u64;
    Ok(NodeIndex {
        node_data_start,
        source_size: upto,
        source_mtime_sec: 0,
        source_mtime_nsec: 0,
        indexed_upto: upto,
        prefix_guard: guard_of(data, upto),
        roots,
        assign,
        reverse,
        children: child_map,
        revs: Vec::new(),
    })
}

/// FNV-1a：用来做前缀守护（检测「索引落后且源文件被改写」）。
pub fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// `upto` 之前 `GUARD_WINDOW` 字节的哈希。
fn guard_of(data: &[u8], upto: u64) -> u64 {
    let upto = (upto as usize).min(data.len());
    let start = upto.saturating_sub(GUARD_WINDOW as usize);
    fnv1a(&data[start..upto])
}

/// 写 sidecar（四个条目块按各自的首个 UUID 字节升序）。
pub fn write(path: &Path, idx: &NodeIndex) -> std::io::Result<()> {
    let header = HEADER.as_bytes();
    let mut out = Vec::new();
    out.extend_from_slice(MAGIC);
    out.push(VERSION);
    out.extend_from_slice(&(header.len() as u32).to_be_bytes());
    out.extend_from_slice(header);
    out.extend_from_slice(&idx.source_size.to_be_bytes());
    out.extend_from_slice(&idx.source_mtime_sec.to_be_bytes());
    out.extend_from_slice(&idx.source_mtime_nsec.to_be_bytes());
    out.extend_from_slice(&idx.node_data_start.to_be_bytes());
    out.extend_from_slice(&idx.indexed_upto.to_be_bytes());
    out.extend_from_slice(&idx.prefix_guard.to_be_bytes());

    let mut roots: Vec<(Uuid, (u64, u64))> = idx.roots.iter().map(|(k, v)| (*k, *v)).collect();
    roots.sort_by(|a, b| a.0 .0.cmp(&b.0 .0));
    // 每个节点在文件里的位置：根在 roots 里，其余在 children 里。
    let mut loc: HashMap<Uuid, (u64, u64)> = HashMap::new();
    for (id, (off, len)) in &idx.roots {
        loc.insert(*id, (*off, *len));
    }
    for cs in idx.children.values() {
        for (child, off, len) in cs {
            loc.insert(*child, (*off, *len));
        }
    }
    let mut assign: Vec<(Uuid, Uuid, u64, u64)> = idx
        .assign
        .iter()
        .map(|(node, root)| {
            let (off, len) = loc.get(node).copied().unwrap_or((0, 0));
            (*node, *root, off, len)
        })
        .collect();
    assign.sort_by(|a, b| a.0 .0.cmp(&b.0 .0));
    let mut edges: Vec<(Uuid, Uuid)> = Vec::new();
    for (target, srcs) in &idx.reverse {
        for s in srcs {
            edges.push((*target, *s));
        }
    }
    edges.sort_by(|a, b| a.0 .0.cmp(&b.0 .0).then_with(|| a.1 .0.cmp(&b.1 .0)));
    let mut kids: Vec<(Uuid, Uuid, u64, u64)> = Vec::new();
    for (parent, cs) in &idx.children {
        for (child, off, len) in cs {
            kids.push((*parent, *child, *off, *len));
        }
    }
    // 同一个父节点下按「文件内偏移」排 = 追加序，保证并集结果稳定。
    kids.sort_by(|a, b| a.0 .0.cmp(&b.0 .0).then_with(|| a.2.cmp(&b.2)));

    out.extend_from_slice(&(roots.len() as u64).to_be_bytes());
    out.extend_from_slice(&(assign.len() as u64).to_be_bytes());
    out.extend_from_slice(&(edges.len() as u64).to_be_bytes());
    out.extend_from_slice(&(kids.len() as u64).to_be_bytes());
    out.extend_from_slice(&(idx.revs.len() as u64).to_be_bytes());

    for (id, (off, len)) in &roots {
        out.extend_from_slice(&id.0);
        out.extend_from_slice(&off.to_be_bytes());
        out.extend_from_slice(&len.to_be_bytes());
    }
    for (node, root, off, len) in &assign {
        out.extend_from_slice(&node.0);
        out.extend_from_slice(&root.0);
        out.extend_from_slice(&off.to_be_bytes());
        out.extend_from_slice(&len.to_be_bytes());
    }
    for (target, source) in &edges {
        out.extend_from_slice(&target.0);
        out.extend_from_slice(&source.0);
    }
    for (parent, child, off, len) in &kids {
        out.extend_from_slice(&parent.0);
        out.extend_from_slice(&child.0);
        out.extend_from_slice(&off.to_be_bytes());
        out.extend_from_slice(&len.to_be_bytes());
    }
    for r in &idx.revs {
        out.extend_from_slice(&rev_bytes(r));
    }
    std::fs::write(path, out)
}

/// 固定字段区的起始偏移（魔数 5 + 版本 1 + 头长 4 + 头文本）。
pub fn fields_offset() -> u64 {
    5 + 1 + 4 + HEADER.len() as u64
}

fn rev_bytes(r: &RevEntry) -> Vec<u8> {
    let mut b = Vec::with_capacity(REV_SIZE as usize);
    b.extend_from_slice(&r.id.0);
    b.extend_from_slice(&r.parent.unwrap_or(codec::NIL_UUID).0);
    b.extend_from_slice(&r.root.0);
    b.extend_from_slice(&r.rel_off.to_be_bytes());
    b.extend_from_slice(&r.len.to_be_bytes());
    b.extend_from_slice(&r.ref_target.unwrap_or(codec::NIL_UUID).0);
    b
}

fn read_rev(file: &mut std::fs::File, off: u64) -> Result<RevEntry, String> {
    let id = read_uuid_at(file, off)?;
    let p = read_uuid_at(file, off + 16)?;
    let root = read_uuid_at(file, off + 32)?;
    let rel_off = read_u64_at(file, off + 48)?;
    let len = read_u64_at(file, off + 56)?;
    let t = read_uuid_at(file, off + 64)?;
    Ok(RevEntry {
        id,
        parent: if p.is_nil() { None } else { Some(p) },
        root,
        rel_off,
        len,
        ref_target: if t.is_nil() { None } else { Some(t) },
    })
}

fn read_u32(file: &mut std::fs::File) -> Result<u32, String> {
    let mut b = [0u8; 4];
    file.read_exact(&mut b).map_err(|e| e.to_string())?;
    Ok(u32::from_be_bytes(b))
}

fn read_i64(file: &mut std::fs::File) -> Result<i64, String> {
    let mut b = [0u8; 8];
    file.read_exact(&mut b).map_err(|e| e.to_string())?;
    Ok(i64::from_be_bytes(b))
}

fn read_u64(file: &mut std::fs::File) -> Result<u64, String> {
    let mut b = [0u8; 8];
    file.read_exact(&mut b).map_err(|e| e.to_string())?;
    Ok(u64::from_be_bytes(b))
}

fn read_uuid_at(file: &mut std::fs::File, off: u64) -> Result<Uuid, String> {
    file.seek(SeekFrom::Start(off)).map_err(|e| e.to_string())?;
    let mut b = [0u8; 16];
    file.read_exact(&mut b).map_err(|e| e.to_string())?;
    Ok(Uuid(b))
}

fn read_u64_at(file: &mut std::fs::File, off: u64) -> Result<u64, String> {
    file.seek(SeekFrom::Start(off)).map_err(|e| e.to_string())?;
    read_u64(file)
}

/// 在按 UUID 升序的定长块上二分查找，返回条目下标或 None。
fn bsearch_uuid(
    file: &mut std::fs::File,
    block_off: u64,
    entry_size: u64,
    count: u64,
    target: Uuid,
) -> Result<Option<u64>, String> {
    let mut lo = 0u64;
    let mut hi = count;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let k = read_uuid_at(file, block_off + mid * entry_size)?;
        match k.0.cmp(&target.0) {
            std::cmp::Ordering::Less => lo = mid + 1,
            std::cmp::Ordering::Greater => hi = mid,
            std::cmp::Ordering::Equal => return Ok(Some(mid)),
        }
    }
    Ok(None)
}

/// 在按 UUID 升序的定长块上二分查找下界（第一个 key >= target 的下标）。
fn lower_bound_uuid(
    file: &mut std::fs::File,
    block_off: u64,
    entry_size: u64,
    count: u64,
    target: Uuid,
) -> Result<u64, String> {
    let mut lo = 0u64;
    let mut hi = count;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let k = read_uuid_at(file, block_off + mid * entry_size)?;
        if k.0 < target.0 {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    Ok(lo)
}

/// 侧车索引是否与源文件同步（尺寸 / mtime 一致）——用于 `xr index` 报准确的「复用 / 重建」。
pub fn is_fresh(path: &Path) -> bool {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return false,
    };
    let (sec, nsec) = mtime(&meta);
    match Sidecar::open(&sidecar_path(path)) {
        Ok(s) => {
            s.source_size == meta.len()
                && s.source_mtime_sec == sec
                && s.source_mtime_nsec == nsec
        }
        Err(_) => false,
    }
}

/// 落盘的 sidecar 只读句柄：不把索引读进内存，按需 seek + 二分查找。
pub struct Sidecar {
    file: std::fs::File,
    pub node_data_start: u64,
    pub source_size: u64,
    pub source_mtime_sec: i64,
    pub source_mtime_nsec: u32,
    /// 已经扫描到源文件的哪个绝对偏移；之后的字节由 rev 块补齐。
    pub indexed_upto: u64,
    pub prefix_guard: u64,
    pub root_count: u64,
    pub node_count: u64,
    pub edge_count: u64,
    pub child_count: u64,
    pub rev_count: u64,
    root_off: u64,
    assign_off: u64,
    reverse_off: u64,
    children_off: u64,
    /// 追加修订（按写入顺序）；同一编号取最后一条。
    revs: Vec<RevEntry>,
    rev_by_id: HashMap<Uuid, usize>,
    rev_children: HashMap<Uuid, Vec<usize>>,
    /// 根的当前字节范围（含追加带来的增长 / 新增根）。
    rev_roots: HashMap<Uuid, (u64, u64)>,
}

impl Sidecar {
    pub fn open(path: &Path) -> Result<Sidecar, String> {
        let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
        let mut magic = [0u8; 5];
        file.read_exact(&mut magic).map_err(|e| e.to_string())?;
        if &magic != MAGIC {
            return Err("F005：索引魔数非法".into());
        }
        let mut ver = [0u8; 1];
        file.read_exact(&mut ver).map_err(|e| e.to_string())?;
        if ver[0] != VERSION {
            return Err("F006：索引版本不支持".into());
        }
        let hlen = read_u32(&mut file)? as usize;
        let mut hbuf = vec![0u8; hlen];
        file.read_exact(&mut hbuf).map_err(|e| e.to_string())?;
        let source_size = read_u64(&mut file)?;
        let source_mtime_sec = read_i64(&mut file)?;
        let source_mtime_nsec = read_u32(&mut file)?;
        let node_data_start = read_u64(&mut file)?;
        let indexed_upto = read_u64(&mut file)?;
        let prefix_guard = read_u64(&mut file)?;
        let root_count = read_u64(&mut file)?;
        let node_count = read_u64(&mut file)?;
        let edge_count = read_u64(&mut file)?;
        let child_count = read_u64(&mut file)?;
        let rev_count = read_u64(&mut file)?;
        let root_off = file.stream_position().map_err(|e| e.to_string())?;
        let assign_off = root_off + root_count * 32;
        let reverse_off = assign_off + node_count * 48;
        let children_off = reverse_off + edge_count * 32;
        let rev_off = children_off + child_count * 48;

        let mut revs = Vec::new();
        for i in 0..rev_count {
            revs.push(read_rev(&mut file, rev_off + i * REV_SIZE)?);
        }
        let mut rev_by_id = HashMap::new();
        let mut rev_children: HashMap<Uuid, Vec<usize>> = HashMap::new();
        let mut rev_roots: HashMap<Uuid, (u64, u64)> = HashMap::new();
        for (i, r) in revs.iter().enumerate() {
            rev_by_id.insert(r.id, i);
            if let Some(p) = r.parent {
                rev_children.entry(p).or_default().push(i);
            }
            let end = r.rel_off + r.len;
            match rev_roots.get(&r.root).copied() {
                Some((start, len)) => {
                    let new_len = len.max(end.saturating_sub(start));
                    rev_roots.insert(r.root, (start, new_len));
                }
                None => {
                    rev_roots.insert(r.root, (r.rel_off, r.len));
                }
            }
        }

        Ok(Sidecar {
            file,
            node_data_start,
            source_size,
            source_mtime_sec,
            source_mtime_nsec,
            indexed_upto,
            prefix_guard,
            root_count,
            node_count,
            edge_count,
            child_count,
            rev_count,
            root_off,
            assign_off,
            reverse_off,
            children_off,
            revs,
            rev_by_id,
            rev_children,
            rev_roots,
        })
    }

    /// 打开 sidecar；缺索引或源文件尺寸 / mtime 不匹配则重建后打开。
    pub fn open_for(path: &Path) -> Result<Sidecar, String> {
        let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
        let size = meta.len();
        let (sec, nsec) = mtime(&meta);
        let sp = sidecar_path(path);
        if let Ok(s) = Sidecar::open(&sp) {
            if s.source_size == size && s.source_mtime_sec == sec && s.source_mtime_nsec == nsec {
                return Ok(s);
            }
            // 只增长（append-v1 编辑）→ 只扫新增的那一段，不整份重建。
            if size > s.source_size && s.indexed_upto <= size {
                drop(s);
                if extend(path).is_ok() {
                    return Sidecar::open(&sp);
                }
            }
        }
        rebuild(path)?;
        Sidecar::open(&sp)
    }

    pub fn find_root(&mut self, id: Uuid) -> Result<Option<(u64, u64)>, String> {
        if let Some(v) = self.rev_roots.get(&id) {
            return Ok(Some(*v));
        }
        let idx = match bsearch_uuid(&mut self.file, self.root_off, 32, self.root_count, id)? {
            Some(i) => i,
            None => return Ok(None),
        };
        let off = self.root_off + idx * 32 + 16;
        Ok(Some((read_u64_at(&mut self.file, off)?, read_u64_at(&mut self.file, off + 8)?)))
    }

    /// 取任意一个根（只读一条记录）：给「打开文件后先显示一棵树」用。
    pub fn find_root_any(&mut self) -> Result<Option<Uuid>, String> {
        if self.root_count > 0 {
            return Ok(Some(read_uuid_at(&mut self.file, self.root_off)?));
        }
        Ok(self.rev_roots.keys().next().copied())
    }

    /// 全部根编号（基础块 + 追加进来的新根），只读编号、不解码节点。
    pub fn all_roots(&mut self) -> Result<Vec<Uuid>, String> {
        let mut out = Vec::new();
        for i in 0..self.root_count {
            out.push(read_uuid_at(&mut self.file, self.root_off + i * 32)?);
        }
        let mut extra: Vec<Uuid> = self.rev_roots.keys().copied().collect();
        extra.sort_by(|a, b| a.0.cmp(&b.0));
        for id in extra {
            if !out.contains(&id) {
                out.push(id);
            }
        }
        Ok(out)
    }

    pub fn find_assign(&mut self, id: Uuid) -> Result<Option<Uuid>, String> {
        if let Some(&ri) = self.rev_by_id.get(&id) {
            return Ok(Some(self.revs[ri].root));
        }
        let idx = match bsearch_uuid(&mut self.file, self.assign_off, 48, self.node_count, id)? {
            Some(i) => i,
            None => return Ok(None),
        };
        Ok(Some(read_uuid_at(&mut self.file, self.assign_off + idx * 48 + 16)?))
    }

    /// 任意节点在源文件里的位置 (相对偏移, 字节长)——用来按偏移直读单个节点。
    pub fn find_node_loc(&mut self, id: Uuid) -> Result<Option<(u64, u64)>, String> {
        if let Some(&ri) = self.rev_by_id.get(&id) {
            let e = self.revs[ri];
            return Ok(Some((e.rel_off, e.len)));
        }
        let idx = match bsearch_uuid(&mut self.file, self.assign_off, 48, self.node_count, id)? {
            Some(i) => i,
            None => return Ok(None),
        };
        let off = self.assign_off + idx * 48 + 32;
        Ok(Some((read_u64_at(&mut self.file, off)?, read_u64_at(&mut self.file, off + 8)?)))
    }

    pub fn find_reverse(&mut self, id: Uuid) -> Result<Vec<Uuid>, String> {
        let start = lower_bound_uuid(&mut self.file, self.reverse_off, 32, self.edge_count, id)?;
        let mut out = Vec::new();
        for i in start..self.edge_count {
            let off = self.reverse_off + i * 32;
            let t = read_uuid_at(&mut self.file, off)?;
            if t.0 != id.0 {
                break;
            }
            out.push(read_uuid_at(&mut self.file, off + 16)?);
        }
        // 后写覆盖：基础边若已被修订改掉（不再指向 id）则剔除；修订新加的边补进来。
        out.retain(|s| match self.rev_by_id.get(s) {
            Some(&ri) => self.revs[ri].ref_target == Some(id),
            None => true,
        });
        let mut seen: HashSet<Uuid> = out.iter().copied().collect();
        let mut extra: Vec<Uuid> = Vec::new();
        for i in 0..self.revs.len() {
            let r = self.revs[i];
            if self.rev_by_id.get(&r.id) != Some(&i) {
                continue; // 只算该编号的最后一条
            }
            if r.ref_target == Some(id) && seen.insert(r.id) {
                extra.push(r.id);
            }
        }
        extra.sort_by(|a, b| a.0.cmp(&b.0));
        out.extend(extra);
        Ok(out)
    }

    /// 某个父节点的直接孩子：返回 [(子 UUID, 相对偏移, 字节长)]，按**文件内追加序**。
    /// 每条约 48 字节，用磁盘二分定位，不读整份索引。
    pub fn find_children(&mut self, id: Uuid) -> Result<Vec<(Uuid, u64, u64)>, String> {
        let start = lower_bound_uuid(&mut self.file, self.children_off, 48, self.child_count, id)?;
        let mut out: Vec<(Uuid, u64, u64)> = Vec::new();
        let mut seen: HashSet<Uuid> = HashSet::new();
        for i in start..self.child_count {
            let off = self.children_off + i * 48;
            let parent = read_uuid_at(&mut self.file, off)?;
            if parent.0 != id.0 {
                break;
            }
            let child = read_uuid_at(&mut self.file, off + 16)?;
            let rel = read_u64_at(&mut self.file, off + 32)?;
            let len = read_u64_at(&mut self.file, off + 40)?;
            if !seen.insert(child) {
                continue;
            }
            // 孩子在修订里被搬走了 / 改大了，就按最后一条记录算。
            match self.rev_by_id.get(&child) {
                Some(&ri) => {
                    let e = self.revs[ri];
                    if e.parent != Some(id) {
                        continue;
                    }
                    out.push((child, e.rel_off, e.len));
                }
                None => out.push((child, rel, len)),
            }
        }
        // 追加进来的新孩子（基础块里没有）。
        if let Some(list) = self.rev_children.get(&id) {
            for &ri in list {
                let e = self.revs[ri];
                if self.rev_by_id.get(&e.id) != Some(&ri) || !seen.insert(e.id) {
                    continue;
                }
                out.push((e.id, e.rel_off, e.len));
            }
        }
        out.sort_by_key(|(_, rel, _)| *rel);
        Ok(out)
    }

}

/// 总是读源文件重建并落盘。
pub fn rebuild(path: &Path) -> Result<(), String> {
    let data = std::fs::read(path).map_err(|e| e.to_string())?;
    let mut idx = build(&data)?;
    if let Ok(meta) = std::fs::metadata(path) {
        let (sec, nsec) = mtime(&meta);
        idx.source_size = meta.len();
        idx.source_mtime_sec = sec;
        idx.source_mtime_nsec = nsec;
    }
    write(&sidecar_path(path), &idx).map_err(|e| e.to_string())?;
    Ok(())
}

/// 写穿（供 `Store::save` 调用）：源文件已落盘，重建 sidecar。失败静默（缓存可重建）。
pub fn write_for(path: &Path, data: &[u8]) {
    if let Ok(mut idx) = build(data) {
        if let Ok(meta) = std::fs::metadata(path) {
            let (sec, nsec) = mtime(&meta);
            idx.source_size = meta.len();
            idx.source_mtime_sec = sec;
            idx.source_mtime_nsec = nsec;
        }
        let _ = write(&sidecar_path(path), &idx);
    }
}

/// 增量补扫的结果。
#[derive(Debug, Default, Clone, Copy)]
pub struct ExtendReport {
    /// 本次新增 / 改写的记录条数。
    pub new_records: usize,
    /// 扫到的最后一个完整记录边界（绝对偏移）。
    pub good_end: u64,
    /// 尾部残片字节数（追加中断留下的半条记录，> 0 表示需要截断）。
    pub truncated_bytes: u64,
}

/// 前缀守护：`upto` 之前 `GUARD_WINDOW` 字节的哈希（按需读文件）。
fn guard_at(path: &Path, upto: u64) -> Result<u64, String> {
    let mut f = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let start = upto.saturating_sub(GUARD_WINDOW);
    let len = (upto - start) as usize;
    if len == 0 {
        return Ok(fnv1a(&[]));
    }
    f.seek(SeekFrom::Start(start)).map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; len];
    f.read_exact(&mut buf).map_err(|e| e.to_string())?;
    Ok(fnv1a(&buf))
}

/// 只扫源文件「已扫过位置之后」的新增字节，把结果作为修订条目追加到 sidecar 尾部。
///
/// 这是 `append-v1` 的读侧关键：改一个词只追加几十字节，索引也只补扫那几十字节，
/// 不重建 318 MB 的索引。源文件若被从中间改写（前缀守护对不上）则返回 Err，
/// 由调用方退回整份重建。
pub fn extend(path: &Path) -> Result<ExtendReport, String> {
    let sp = sidecar_path(path);
    let mut sc = Sidecar::open(&sp)?;
    let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
    let size = meta.len();
    if size < sc.source_size {
        return Err("源文件变小（F004 截断），需要整份重建索引".into());
    }
    if size == sc.source_size {
        return Ok(ExtendReport {
            new_records: 0,
            good_end: sc.indexed_upto,
            truncated_bytes: 0,
        });
    }
    let upto = sc.indexed_upto.min(sc.source_size);
    if guard_at(path, upto)? != sc.prefix_guard {
        return Err("索引前缀校验失败（源文件被改写），需要整份重建索引".into());
    }

    // 读入新增区间
    let mut f = std::fs::File::open(path).map_err(|e| e.to_string())?;
    f.seek(SeekFrom::Start(upto)).map_err(|e| e.to_string())?;
    let mut tail = Vec::new();
    f.read_to_end(&mut tail).map_err(|e| e.to_string())?;

    let mut off = 0usize;
    let mut new_revs: Vec<RevEntry> = Vec::new();
    let mut good_tail = 0usize;
    while off < tail.len() {
        let rec_start = off;
        match scan_node(&tail, &mut off) {
            Ok(rec) => {
                good_tail = off;
                let root = match rec.parent {
                    None => rec.id,
                    Some(p) => sc
                        .find_assign(p)
                        .unwrap_or(None)
                        .unwrap_or(rec.id),
                };
                new_revs.push(RevEntry {
                    id: rec.id,
                    parent: rec.parent,
                    root,
                    rel_off: (upto - sc.node_data_start) + rec_start as u64,
                    len: (rec.end - rec.start) as u64,
                    ref_target: rec.ref_target,
                });
            }
            Err(_) => break, // 尾部残片：交给调用方截断
        }
    }

    if new_revs.is_empty() {
        let (sec, nsec) = mtime(&meta);
        let _ = update_header(
            &sp,
            size,
            sec,
            nsec,
            upto + good_tail as u64,
            guard_at(path, upto + good_tail as u64)?,
            sc.rev_count,
        );
        return Ok(ExtendReport {
            new_records: 0,
            good_end: upto + good_tail as u64,
            truncated_bytes: (tail.len() - good_tail) as u64,
        });
    }

    // 追加修订条目到 sidecar 尾部
    let mut out = Vec::new();
    for r in &new_revs {
        out.extend_from_slice(&rev_bytes(r));
    }
    {
        let mut sf = std::fs::OpenOptions::new()
            .write(true)
            .open(&sp)
            .map_err(|e| e.to_string())?;
        sf.seek(SeekFrom::End(0)).map_err(|e| e.to_string())?;
        use std::io::Write;
        sf.write_all(&out).map_err(|e| e.to_string())?;
        sf.sync_all().map_err(|e| e.to_string())?;
    }

    let good_end = upto + good_tail as u64;
    let (sec, nsec) = mtime(&meta);
    update_header(
        &sp,
        size,
        sec,
        nsec,
        good_end,
        guard_at(path, good_end)?,
        sc.rev_count + new_revs.len() as u64,
    )?;

    Ok(ExtendReport {
        new_records: new_revs.len(),
        good_end,
        truncated_bytes: (tail.len() - good_tail) as u64,
    })
}

/// 就地改写 sidecar 头部字段（长度不变，不需要重排数据块）。
fn update_header(
    sp: &Path,
    source_size: u64,
    mtime_sec: i64,
    mtime_nsec: u32,
    indexed_upto: u64,
    prefix_guard: u64,
    rev_count: u64,
) -> Result<(), String> {
    use std::io::Write;
    let mut sf = std::fs::OpenOptions::new()
        .write(true)
        .open(sp)
        .map_err(|e| e.to_string())?;
    let base = fields_offset();
    sf.seek(SeekFrom::Start(base))
        .map_err(|e| e.to_string())?;
    sf.write_all(&source_size.to_be_bytes()).map_err(|e| e.to_string())?;
    sf.write_all(&mtime_sec.to_be_bytes()).map_err(|e| e.to_string())?;
    sf.write_all(&mtime_nsec.to_be_bytes()).map_err(|e| e.to_string())?;
    // node_data_start 不变：跳过 8 字节，别把它覆盖掉
    sf.seek(SeekFrom::Current(8)).map_err(|e| e.to_string())?;
    sf.write_all(&indexed_upto.to_be_bytes()).map_err(|e| e.to_string())?;
    sf.write_all(&prefix_guard.to_be_bytes()).map_err(|e| e.to_string())?;
    // 跳过 root/node/edge/child 四个计数，直接改 rev_count
    sf.seek(SeekFrom::Current(32)).map_err(|e| e.to_string())?;
    sf.write_all(&rev_count.to_be_bytes()).map_err(|e| e.to_string())?;
    sf.sync_all().map_err(|e| e.to_string())
}

/// 追加写入器：只往文件末尾追加节点记录，并顺带把索引补扫上去。
pub struct AppendWriter {
    path: PathBuf,
    file: std::fs::File,
    appended: Vec<RevEntry>,
    node_data_start: u64,
}

impl AppendWriter {
    /// 打开（先补齐索引；尾部残片会被截断并报告）。
    pub fn open(path: &Path) -> Result<(AppendWriter, ExtendReport), String> {
        let mut report = ExtendReport::default();
        if sidecar_path(path).exists() {
            if let Ok(r) = extend(path) {
                report = r;
            } else {
                rebuild(path)?;
            }
        } else {
            rebuild(path)?;
        }
        // 尾部残片（F011）：截断到最后一条完整记录
        if report.truncated_bytes > 0 {
            let f = std::fs::OpenOptions::new()
                .write(true)
                .open(path)
                .map_err(|e| e.to_string())?;
            f.set_len(report.good_end).map_err(|e| e.to_string())?;
            f.sync_all().map_err(|e| e.to_string())?;
            rebuild(path)?;
        }
        let head = {
            let mut f = std::fs::File::open(path).map_err(|e| e.to_string())?;
            let mut buf = [0u8; 9];
            f.read_exact(&mut buf).map_err(|e| e.to_string())?;
            let n = u32::from_be_bytes(buf[5..9].try_into().unwrap()) as usize;
            let mut head = vec![0u8; 9 + n];
            f.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
            f.read_exact(&mut head).map_err(|e| e.to_string())?;
            head
        };
        let node_data_start =
            tree::node_data_start(&head).map_err(|e| e.to_string())?;
        let file = std::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .map_err(|e| e.to_string())?;
        Ok((
            AppendWriter {
                path: path.to_path_buf(),
                file,
                appended: Vec::new(),
                node_data_start,
            },
            report,
        ))
    }

    /// 追加一条记录（同编号 = 修订，后写覆盖）。
    pub fn append_node(&mut self, node: &Node) -> Result<(), String> {
        use std::io::Write;
        let bytes = codec::encode_node(node).map_err(|e| format!("{e:?}"))?;
        let start = self
            .file
            .seek(SeekFrom::End(0))
            .map_err(|e| e.to_string())?;
        self.file.write_all(&bytes).map_err(|e| e.to_string())?;
        self.appended.push(RevEntry {
            id: node.id,
            parent: node.parent,
            root: node.parent.unwrap_or(node.id),
            rel_off: start - self.node_data_start,
            len: bytes.len() as u64,
            ref_target: match node.value {
                codec::Value::Reference(t) => Some(t),
                _ => None,
            },
        });
        Ok(())
    }

    /// 落盘并补扫索引，返回本次补扫报告。
    pub fn sync(&mut self) -> Result<ExtendReport, String> {
        self.file.sync_all().map_err(|e| e.to_string())?;
        let r = extend(&self.path);
        self.appended.clear();
        r
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 尚未补扫的追加条数。
    pub fn pending(&self) -> usize {
        self.appended.len()
    }
}

/// 合并（compact）：折叠掉同一编号的历史记录，重写文件 + 重建索引。
pub fn compact_file(path: &Path) -> Result<(usize, usize), String> {
    let raw = Store::load(path)?;
    let folded = tree::fold(&raw);
    folded.save(path).map_err(|e| e.to_string())?;
    Ok((raw.len(), folded.len()))
}

/// 从源文件按「相对偏移 + 字节长」读出单个节点（只读这一段，不整读文件）。
pub fn read_node_at(
    path: &Path,
    node_data_start: u64,
    rel_off: u64,
    len: u64,
) -> Result<Node, String> {
    let mut f = std::fs::File::open(path).map_err(|e| e.to_string())?;
    f.seek(SeekFrom::Start(node_data_start + rel_off))
        .map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; len as usize];
    f.read_exact(&mut buf).map_err(|e| e.to_string())?;
    let mut off = 0usize;
    crate::codec::decode_node(&buf, &mut off).map_err(tree::codec_error)
}

/// 跨文件工作区：只索引 + 按偏移读，节点数据不常驻；支持「同编号多文件」取并集。
#[derive(Default)]
pub struct LazyWorkspace {
    files: Vec<(String, Sidecar)>,
    subtree_cache: HashMap<Uuid, Store>,
}

impl LazyWorkspace {
    pub fn from_paths(paths: &[String]) -> Result<Self, String> {
        let mut files = Vec::new();
        for p in paths {
            let sidecar = Sidecar::open_for(Path::new(p))?;
            files.push((p.clone(), sidecar));
        }
        Ok(Self { files, subtree_cache: HashMap::new() })
    }

    fn ensure_subtree(&mut self, file_idx: usize, root_id: Uuid) -> Result<(), String> {
        if self.subtree_cache.contains_key(&root_id) {
            return Ok(());
        }
        if let Err(e) = self.read_subtree(file_idx, root_id) {
            // 索引偏移失效：重建该文件索引后重试一次。
            let path = self.files[file_idx].0.clone();
            rebuild(Path::new(&path))?;
            self.files[file_idx].1 = Sidecar::open(&sidecar_path(Path::new(&path)))?;
            self.subtree_cache.clear();
            self.read_subtree(file_idx, root_id).map_err(|_| e)?;
        }
        Ok(())
    }

    fn read_subtree(&mut self, file_idx: usize, root_id: Uuid) -> Result<(), String> {
        let (rel_off, len, node_data_start, path) = {
            let (path, sc) = &mut self.files[file_idx];
            let (rel_off, len) = sc.find_root(root_id)?.ok_or("F008：索引与源文件不一致")?;
            (rel_off, len, sc.node_data_start, path.clone())
        };
        let abs = node_data_start + rel_off;
        let mut f = std::fs::File::open(&path).map_err(|e| e.to_string())?;
        let mut buf = vec![0u8; len as usize];
        f.seek(SeekFrom::Start(abs)).map_err(|e| e.to_string())?;
        f.read_exact(&mut buf).map_err(|e| e.to_string())?;
        let store = Store::decode(&buf).map_err(tree::codec_error)?;
        let root = store.get(root_id).cloned().ok_or("F008：索引与源文件不一致")?;
        self.subtree_cache.insert(root_id, store.sub_store(&root));
        Ok(())
    }

    /// 按 UUID 定位并懒加载目标节点，返回 (文件路径, 节点)。
    pub fn find(&mut self, id: Uuid) -> Option<(String, Node)> {
        for fi in 0..self.files.len() {
            let root_id = match self.files[fi].1.find_assign(id) {
                Ok(Some(r)) => r,
                _ => continue,
            };
            if self.ensure_subtree(fi, root_id).is_err() {
                continue;
            }
            if let Some(node) = self.subtree_cache.get(&root_id).and_then(|s| s.get(id)).cloned() {
                return Some((self.files[fi].0.clone(), node));
            }
        }
        None
    }

    /// 该编号在**每个相关文件**里的一份（名字/值 + 来源文件），按文件顺序。
    /// 「同编号多文件」是正常现象，所以这里是「多份」而不是「一份」。
    pub fn node_views(&mut self, id: Uuid) -> Vec<(String, Node)> {
        let mut out = Vec::new();
        for fi in 0..self.files.len() {
            let loc = self.files[fi].1.find_node_loc(id).ok().flatten();
            if let Some((rel, len)) = loc {
                let path = self.files[fi].0.clone();
                let nstart = self.files[fi].1.node_data_start;
                if let Ok(n) = read_node_at(Path::new(&path), nstart, rel, len) {
                    out.push((path, n));
                }
            }
        }
        out
    }

    /// 并集后的孩子：**所有文件里父边指向该编号的节点**，按「文件顺序 + 文件内追加序」，
    /// 同一个孩子编号只算一次（来源取最先出现的那个文件）。
    pub fn children_union(&mut self, id: Uuid) -> Vec<(String, Node)> {
        let mut out = Vec::new();
        let mut seen: HashSet<Uuid> = HashSet::new();
        for fi in 0..self.files.len() {
            let kids = self.files[fi].1.find_children(id).unwrap_or_default();
            for (child, rel, len) in kids {
                if !seen.insert(child) {
                    continue;
                }
                let path = self.files[fi].0.clone();
                let nstart = self.files[fi].1.node_data_start;
                if let Ok(n) = read_node_at(Path::new(&path), nstart, rel, len) {
                    out.push((path, n));
                }
            }
        }
        out
    }

    /// 谁引用了我：跨所有文件聚合反向边，逐个懒加载源节点。
    pub fn references_to(&mut self, id: Uuid) -> Vec<(String, Node)> {
        let mut out = Vec::new();
        for fi in 0..self.files.len() {
            let srcs = match self.files[fi].1.find_reverse(id) {
                Ok(v) => v,
                Err(_) => continue,
            };
            for s in srcs {
                if let Some((path, n)) = self.find(s) {
                    out.push((path, n));
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::Value;

    fn file_bytes(store: &Store) -> Vec<u8> {
        tree::make_file(&store.encode().unwrap())
    }

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("xirang_{name}_{}.xirang", Uuid::random_v4()))
    }

    #[test]
    fn build_roots_assign_reverse() {
        let mut s = Store::new();
        let a = s.create(None, "甲", Value::Empty, false).id;
        s.create(Some(a), "词形", Value::Text("灯".into()), false);
        let b = s.create(None, "乙", Value::Empty, false).id;
        s.create(Some(b), "释义", Value::Text("目标".into()), false);
        let c = s.create(Some(b), "父类", Value::Reference(a), false).id;

        let idx = build(&file_bytes(&s)).unwrap();
        assert!(idx.roots.contains_key(&a));
        assert!(idx.roots.contains_key(&b));
        assert_eq!(idx.assign[&a], a);
        assert_eq!(idx.assign[&b], b);
        let xing = s.nodes().iter().find(|n| n.name == "词形").unwrap().id;
        assert_eq!(idx.assign[&xing], a);
        assert_eq!(idx.reverse[&a], vec![c]);
    }

    #[test]
    fn build_detects_template_and_instance_roots() {
        let mut s = Store::new();
        let tpl = s.create(None, "词条", Value::Empty, false).id;
        s.create(Some(tpl), "@模板", Value::Empty, false);
        s.create(Some(tpl), "词形", Value::Empty, false);
        let inst = s.create(None, "灯", Value::Empty, false).id;
        s.create(Some(inst), "@实例", Value::Empty, false);
        s.create(Some(inst), "@模板", Value::Reference(tpl), false);

        let idx = build(&file_bytes(&s)).unwrap();
        assert!(idx.roots.contains_key(&tpl));
        assert!(idx.roots.contains_key(&inst));
        assert_eq!(idx.assign[&tpl], tpl);
        assert_eq!(idx.assign[&inst], inst);
        let xing = s.nodes().iter().find(|n| n.name == "词形").unwrap().id;
        assert_eq!(idx.assign[&xing], tpl);
    }

    #[test]
    fn sidecar_lookup_roundtrip() {
        let mut s = Store::new();
        let a = s.create(None, "根", Value::Empty, false).id;
        let b = s.create(Some(a), "子", Value::Reference(a), false).id;
        let idx = build(&file_bytes(&s)).unwrap();

        let p = tmp("lookup");
        write(&sidecar_path(&p), &idx).unwrap();
        let mut sc = Sidecar::open(&sidecar_path(&p)).unwrap();
        assert_eq!(sc.find_root(a).unwrap(), Some(idx.roots[&a]));
        assert_eq!(sc.find_assign(b).unwrap(), Some(a));
        assert_eq!(sc.find_reverse(a).unwrap(), vec![b]);
        assert!(sc.find_root(Uuid([9u8; 16])).unwrap().is_none());
        let _ = std::fs::remove_file(&sidecar_path(&p));
    }

    #[test]
    fn stale_index_rebuilds() {
        let p = tmp("stale");
        let mut s = Store::new();
        s.create(None, "根", Value::Empty, false);
        s.save(&p).unwrap();
        assert_eq!(Sidecar::open_for(&p).unwrap().node_count, 1);

        let mut s2 = Store::new();
        s2.create(None, "根", Value::Empty, false);
        s2.create(None, "二", Value::Empty, false);
        std::fs::write(&p, file_bytes(&s2)).unwrap();
        assert_eq!(Sidecar::open_for(&p).unwrap().node_count, 2);

        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(sidecar_path(&p));
    }

    #[test]
    fn lazy_workspace_resolves_non_root_cross_file() {
        let f1 = tmp("ws1");
        let f2 = tmp("ws2");

        let mut s2 = Store::new();
        let b = s2.create(None, "乙", Value::Empty, false).id;
        let child = s2.create(Some(b), "词义", Value::Text("目标".into()), false).id;
        let mut s1 = Store::new();
        let a = s1.create(None, "甲", Value::Empty, false).id;
        let ref_node = s1.create(Some(a), "指向", Value::Reference(child), false).id;

        s1.save(&f1).unwrap();
        s2.save(&f2).unwrap();
        let paths = vec![f1.to_string_lossy().into_owned(), f2.to_string_lossy().into_owned()];
        let mut ws = LazyWorkspace::from_paths(&paths).unwrap();

        let (file1, n) = ws.find(ref_node).unwrap();
        assert_eq!(file1, paths[0]);
        assert_eq!(n.id, ref_node);
        let (file2, target) = ws.find(child).unwrap();
        assert_eq!(file2, paths[1]);
        assert_eq!(target.name, "词义");
        let incoming = ws.references_to(child);
        assert_eq!(incoming.len(), 1);
        assert_eq!(incoming[0].1.id, ref_node);

        let _ = std::fs::remove_file(&f1);
        let _ = std::fs::remove_file(&f2);
        let _ = std::fs::remove_file(sidecar_path(&f1));
        let _ = std::fs::remove_file(sidecar_path(&f2));
    }

    #[test]
    fn node_views_and_children_union_across_files() {
        let f1 = tmp("u1");
        let f2 = tmp("u2");

        // 同一个编号 shared 出现在两个文件里，各自名/值相同（不算冲突），各挂各的孩子。
        let shared = Uuid::random_v4();
        let mut s1 = Store::new();
        let a = s1.create(None, "甲", Value::Empty, false).id;
        s1.add(Node {
            id: shared,
            parent: Some(a),
            name: "共享".into(),
            value: Value::Text("同".into()),
        });
        let kid1 = s1.create(Some(shared), "形态", Value::Text("甲孩子".into()), false).id;
        // 一个「两文件共用的孩子编号」：同一编号在两边都挂到 shared 下，并集里只能算一次。
        let dup_kid = s1.create(Some(shared), "同孩", Value::Text("原".into()), false).id;

        let mut s2 = Store::new();
        let b = s2.create(None, "乙", Value::Empty, false).id;
        s2.add(Node {
            id: shared,
            parent: Some(b),
            name: "共享".into(),
            value: Value::Text("同".into()),
        });
        let kid2 = s2.create(Some(shared), "形态", Value::Text("乙孩子".into()), false).id;
        s2.add(Node {
            id: dup_kid,
            parent: Some(shared),
            name: "同孩".into(),
            value: Value::Text("原".into()),
        });

        s1.save(&f1).unwrap();
        s2.save(&f2).unwrap();
        let paths = vec![f1.to_string_lossy().into_owned(), f2.to_string_lossy().into_owned()];
        let mut ws = LazyWorkspace::from_paths(&paths).unwrap();

        // 两份视图：按文件顺序，各自标来源。
        let views = ws.node_views(shared);
        assert_eq!(views.len(), 2);
        assert_eq!(views[0].0, paths[0]);
        assert_eq!(views[1].0, paths[1]);

        // 孩子并集：文件顺序 + 文件内追加序；dup_kid 只算一次，来源取最先出现的 f1。
        let kids = ws.children_union(shared);
        let ids: Vec<Uuid> = kids.iter().map(|(_, n)| n.id).collect();
        assert_eq!(ids, vec![kid1, dup_kid, kid2]);
        assert_eq!(kids[0].0, paths[0]);
        assert_eq!(kids[1].0, paths[0]); // dup_kid 归 f1
        assert_eq!(kids[2].0, paths[1]);

        let _ = std::fs::remove_file(&f1);
        let _ = std::fs::remove_file(&f2);
        let _ = std::fs::remove_file(sidecar_path(&f1));
        let _ = std::fs::remove_file(sidecar_path(&f2));
    }
}
