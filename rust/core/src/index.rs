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

use crate::codec::{self, Node, Uuid, Value};
use crate::tree::{self, Store};

pub const MAGIC: &[u8; 5] = b"XRIDX";
pub const VERSION: u8 = 2;
pub const HEADER: &str = "\
XiRang sidecar node index (XRIDX) v2
Maps each .xirang node to its containing tree root and byte offset (implementation-layer cache).
Layout (big-endian, all blocks sorted by their first 16-byte UUID):
  magic \"XRIDX\" (5 bytes) + version (1 byte) + header_len u32 + this header
  + source_size u64 + source_mtime_sec i64 + source_mtime_nsec u32
  + node_data_start u64 + root_count u64 + node_count u64 + edge_count u64 + child_count u64
  + root_count x (root_uuid 16B, rel_offset u64, byte_len u64)  sorted by root_uuid
  + node_count x (node_uuid 16B, root_uuid 16B, rel_offset u64, byte_len u64)  sorted by node_uuid
  + edge_count x (target_uuid 16B, source_uuid 16B)             sorted by target_uuid then source_uuid
  + child_count x (parent_uuid 16B, child_uuid 16B, rel_offset u64, byte_len u64)  sorted by parent then child
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
    /// 根 UUID → (相对偏移, 字节长)。绝对偏移 = node_data_start + 相对偏移。
    pub roots: HashMap<Uuid, (u64, u64)>,
    /// 每个节点 UUID → 其所属根 UUID（根自身映射到自己）。
    pub assign: HashMap<Uuid, Uuid>,
    /// 引用目标 UUID → 指向它的源节点 UUID 列表。
    pub reverse: HashMap<Uuid, Vec<Uuid>>,
    /// 父 UUID → [(子 UUID, 相对偏移, 字节长)]。用于跨文件「凑孩子」时直接定位。
    pub children: HashMap<Uuid, Vec<(Uuid, u64, u64)>>,
}

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

    let mut by_id: HashMap<Uuid, usize> = HashMap::with_capacity(n);
    for (i, r) in recs.iter().enumerate() {
        by_id.insert(r.id, i);
    }
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut is_root = vec![false; n];
    for (i, r) in recs.iter().enumerate() {
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
        if is_root[i] {
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
        for &ci in &children[i] {
            if sub_end[ci] > sub_end[i] {
                sub_end[i] = sub_end[ci];
            }
        }
    }

    let mut roots: HashMap<Uuid, (u64, u64)> = HashMap::new();
    for i in 0..n {
        if is_root[i] {
            roots.insert(recs[i].id, (recs[i].start as u64, (sub_end[i] - recs[i].start) as u64));
        }
    }

    let mut reverse: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
    for r in &recs {
        if let Some(t) = r.ref_target {
            reverse.entry(t).or_default().push(r.id);
        }
    }
    for v in reverse.values_mut() {
        v.sort_by(|a, b| a.0.cmp(&b.0));
    }

    // 父 → 孩子（含各自在文件里的偏移与长度）：跨文件「凑孩子」时用来直接定位。
    let mut children: HashMap<Uuid, Vec<(Uuid, u64, u64)>> = HashMap::new();
    for r in &recs {
        if let Some(p) = r.parent {
            children
                .entry(p)
                .or_default()
                .push((r.id, r.start as u64, (r.end - r.start) as u64));
        }
    }
    for v in children.values_mut() {
        v.sort_by(|a, b| a.0 .0.cmp(&b.0 .0));
    }

    Ok(NodeIndex {
        node_data_start,
        source_size: data.len() as u64,
        source_mtime_sec: 0,
        source_mtime_nsec: 0,
        roots,
        assign,
        reverse,
        children,
    })
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
    std::fs::write(path, out)
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
    pub root_count: u64,
    pub node_count: u64,
    pub edge_count: u64,
    pub child_count: u64,
    root_off: u64,
    assign_off: u64,
    reverse_off: u64,
    children_off: u64,
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
        let root_count = read_u64(&mut file)?;
        let node_count = read_u64(&mut file)?;
        let edge_count = read_u64(&mut file)?;
        let child_count = read_u64(&mut file)?;
        let root_off = file.stream_position().map_err(|e| e.to_string())?;
        let assign_off = root_off + root_count * 32;
        let reverse_off = assign_off + node_count * 48;
        let children_off = reverse_off + edge_count * 32;
        Ok(Sidecar {
            file,
            node_data_start,
            source_size,
            source_mtime_sec,
            source_mtime_nsec,
            root_count,
            node_count,
            edge_count,
            child_count,
            root_off,
            assign_off,
            reverse_off,
            children_off,
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
        }
        rebuild(path)?;
        Sidecar::open(&sp)
    }

    pub fn find_root(&mut self, id: Uuid) -> Result<Option<(u64, u64)>, String> {
        let idx = match bsearch_uuid(&mut self.file, self.root_off, 32, self.root_count, id)? {
            Some(i) => i,
            None => return Ok(None),
        };
        let off = self.root_off + idx * 32 + 16;
        Ok(Some((read_u64_at(&mut self.file, off)?, read_u64_at(&mut self.file, off + 8)?)))
    }

    pub fn find_assign(&mut self, id: Uuid) -> Result<Option<Uuid>, String> {
        let idx = match bsearch_uuid(&mut self.file, self.assign_off, 48, self.node_count, id)? {
            Some(i) => i,
            None => return Ok(None),
        };
        Ok(Some(read_uuid_at(&mut self.file, self.assign_off + idx * 48 + 16)?))
    }

    /// 任意节点在源文件里的位置 (相对偏移, 字节长)——用来按偏移直读单个节点。
    pub fn find_node_loc(&mut self, id: Uuid) -> Result<Option<(u64, u64)>, String> {
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
        Ok(out)
    }

    /// 某个父节点的直接孩子：返回 [(子 UUID, 相对偏移, 字节长)]，按**文件内追加序**。
    /// 每条约 48 字节，用磁盘二分定位，不读整份索引。
    pub fn find_children(&mut self, id: Uuid) -> Result<Vec<(Uuid, u64, u64)>, String> {
        let start = lower_bound_uuid(&mut self.file, self.children_off, 48, self.child_count, id)?;
        let mut out = Vec::new();
        for i in start..self.child_count {
            let off = self.children_off + i * 48;
            let parent = read_uuid_at(&mut self.file, off)?;
            if parent.0 != id.0 {
                break;
            }
            let child = read_uuid_at(&mut self.file, off + 16)?;
            let rel = read_u64_at(&mut self.file, off + 32)?;
            let len = read_u64_at(&mut self.file, off + 40)?;
            out.push((child, rel, len));
        }
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

/// 索引模式：工作区三本台账（默认）还是每文件侧车。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexMode {
    Workspace,
    Sidecar,
}

/// 由环境变量 `XIRANG_INDEX_MODE` 决定：`sidecar` / `off` / `0` → 侧车；其余（含未设）→ 工作区台账。
pub fn index_mode() -> IndexMode {
    match std::env::var("XIRANG_INDEX_MODE").ok().as_deref() {
        Some("sidecar") | Some("off") | Some("0") | Some("false") => IndexMode::Sidecar,
        _ => IndexMode::Workspace,
    }
}

/// 是否生成每文件侧车索引（工作区模式不再生成）。
pub fn sidecar_enabled() -> bool {
    index_mode() == IndexMode::Sidecar
}

/// 一处命中：哪个文件 + 节点。
pub type FileNode = (String, Node);

/// 「以某节点为根的整棵子树」（含根本身）——三个后端共用同一套展开语义：
/// 先取自己，再逐层往下取孩子，同一编号只出现一次。
fn bfs_subtree<F>(root: Uuid, mut children: F) -> Vec<FileNode>
where
    F: FnMut(Uuid) -> Vec<FileNode>,
{
    let mut out: Vec<FileNode> = Vec::new();
    let mut seen: HashSet<Uuid> = HashSet::new();
    let mut queue: std::collections::VecDeque<Uuid> = std::collections::VecDeque::new();
    queue.push_back(root);
    seen.insert(root);
    while let Some(id) = queue.pop_front() {
        for (file, node) in children(id) {
            if seen.insert(node.id) {
                out.push((file, node.clone()));
                queue.push_back(node.id);
            }
        }
    }
    out
}

/// 索引后端：三类查询 + 整树 + 统计。三种实现（侧车 / 工作区台账 / 整份载入）共用它，
/// 于是「换索引」只是换一个后端，调用方一行不用改，也方便同场对比。
pub trait Backend {
    /// 编号 → 节点（可能多份：同编号多文件）。
    fn locate(&mut self, id: Uuid) -> Vec<FileNode>;
    /// 孩子（跨文件并集，按孩子编号去重）。
    fn children(&mut self, parent: Uuid) -> Vec<FileNode>;
    /// 谁引用我（来源编号 + 所在文件）。
    fn references(&mut self, target: Uuid) -> Vec<(String, Uuid)>;
    /// 整棵子树（含根）。
    fn subtree(&mut self, root: Uuid) -> Vec<FileNode>;
    fn file_count(&self) -> usize;
    /// 打开了多少次索引 / 数据文件（跨文件开销的代理指标）。
    fn opens(&self) -> u64 {
        0
    }
    /// 后端名字：workspace / sidecar / memory。
    fn kind(&self) -> &'static str;
}

/// 侧车后端：每文件一份 XRIDX，逐个文件查（旧行为）。
#[derive(Default)]
pub struct SidecarBackend {
    files: Vec<(String, Sidecar)>,
}

impl SidecarBackend {
    pub fn open(paths: &[String]) -> Result<Self, String> {
        let mut files = Vec::new();
        for p in paths {
            files.push((p.clone(), Sidecar::open_for(Path::new(p))?));
        }
        Ok(Self { files })
    }
}

impl Backend for SidecarBackend {
    fn locate(&mut self, id: Uuid) -> Vec<FileNode> {
        let mut out = Vec::new();
        for (path, sc) in &mut self.files {
            if let Ok(Some((rel, len))) = sc.find_node_loc(id) {
                if let Ok(n) = read_node_at(Path::new(path), sc.node_data_start, rel, len) {
                    out.push((path.clone(), n));
                }
            }
        }
        out
    }

    fn children(&mut self, parent: Uuid) -> Vec<FileNode> {
        let mut out = Vec::new();
        let mut seen: HashSet<Uuid> = HashSet::new();
        for (path, sc) in &mut self.files {
            let kids = sc.find_children(parent).unwrap_or_default();
            for (child, rel, len) in kids {
                if !seen.insert(child) {
                    continue;
                }
                if let Ok(n) = read_node_at(Path::new(path), sc.node_data_start, rel, len) {
                    out.push((path.clone(), n));
                }
            }
        }
        out
    }

    fn references(&mut self, target: Uuid) -> Vec<(String, Uuid)> {
        let mut out = Vec::new();
        for (path, sc) in &mut self.files {
            if let Ok(srcs) = sc.find_reverse(target) {
                for s in srcs {
                    out.push((path.clone(), s));
                }
            }
        }
        out
    }

    fn subtree(&mut self, root: Uuid) -> Vec<FileNode> {
        // 根自己 + 逐层孩子（孩子由侧车的父子块给出，跨文件）
        let mut out: Vec<FileNode> = self.locate(root);
        out.extend(bfs_subtree(root, |id| self.children(id)));
        let mut seen = HashSet::new();
        out.retain(|(_, n)| seen.insert(n.id));
        out
    }

    fn file_count(&self) -> usize {
        self.files.len()
    }

    fn kind(&self) -> &'static str {
        "sidecar"
    }
}

/// 工作区台账后端：三本 wsidx 台账，一次二分命中，不逐个文件扫。
pub struct WorkspaceBackend {
    reader: crate::wsidx::Reader,
}

impl WorkspaceBackend {
    pub fn open(ws_root: &Path) -> Result<Self, String> {
        Ok(Self { reader: crate::wsidx::Reader::open(ws_root)? })
    }
}

impl Backend for WorkspaceBackend {
    fn locate(&mut self, id: Uuid) -> Vec<FileNode> {
        let hits = match self.reader.locate(id) {
            Ok(h) => h,
            Err(_) => return Vec::new(),
        };
        hits.into_iter()
            .filter_map(|h| crate::wsidx::read_node_at_hit(&h).ok().map(|n| (h.file, n)))
            .collect()
    }

    fn children(&mut self, parent: Uuid) -> Vec<FileNode> {
        let hits = match self.reader.children_of(parent) {
            Ok(h) => h,
            Err(_) => return Vec::new(),
        };
        hits.into_iter()
            .filter_map(|h| crate::wsidx::read_node_at_hit(&h).ok().map(|n| (h.file, n)))
            .collect()
    }

    fn references(&mut self, target: Uuid) -> Vec<(String, Uuid)> {
        self.reader.references(target).unwrap_or_default()
    }

    fn subtree(&mut self, root: Uuid) -> Vec<FileNode> {
        let mut out: Vec<FileNode> = self.locate(root);
        out.extend(bfs_subtree(root, |id| self.children(id)));
        let mut seen = HashSet::new();
        out.retain(|(_, n)| seen.insert(n.id));
        out
    }

    fn file_count(&self) -> usize {
        self.reader.file_count()
    }

    fn opens(&self) -> u64 {
        self.reader.opens
    }

    fn kind(&self) -> &'static str {
        "workspace"
    }
}

/// 整份载入后端：索引缺失或对不上时的回退（也用作基准里的内存基线）。
#[derive(Default)]
pub struct MemoryBackend {
    files: Vec<(String, Store)>,
    /// 反向索引：载入时一次建好（「全部进内存」的用法本来就会这么干）
    reverse: HashMap<Uuid, Vec<(String, Uuid)>>,
}

impl MemoryBackend {
    pub fn load(paths: &[String]) -> Result<Self, String> {
        let mut files = Vec::new();
        let mut reverse: HashMap<Uuid, Vec<(String, Uuid)>> = HashMap::new();
        for p in paths {
            let store = Store::load(Path::new(p))?;
            for n in store.nodes() {
                if let Value::Reference(t) = &n.value {
                    reverse.entry(*t).or_default().push((p.clone(), n.id));
                }
            }
            files.push((p.clone(), store));
        }
        Ok(Self { files, reverse })
    }
}

impl Backend for MemoryBackend {
    fn locate(&mut self, id: Uuid) -> Vec<FileNode> {
        self.files
            .iter()
            .filter_map(|(p, s)| s.get(id).map(|n| (p.clone(), n.clone())))
            .collect()
    }

    fn children(&mut self, parent: Uuid) -> Vec<FileNode> {
        let mut out = Vec::new();
        let mut seen: HashSet<Uuid> = HashSet::new();
        for (p, s) in &self.files {
            let Some(node) = s.get(parent) else { continue };
            for c in s.children(node) {
                if seen.insert(c.id) {
                    out.push((p.clone(), c.clone()));
                }
            }
        }
        out
    }

    fn references(&mut self, target: Uuid) -> Vec<(String, Uuid)> {
        self.reverse.get(&target).cloned().unwrap_or_default()
    }

    fn subtree(&mut self, root: Uuid) -> Vec<FileNode> {
        let mut out: Vec<FileNode> = self.locate(root);
        out.extend(bfs_subtree(root, |id| self.children(id)));
        let mut seen = HashSet::new();
        out.retain(|(_, n)| seen.insert(n.id));
        out
    }

    fn file_count(&self) -> usize {
        self.files.len()
    }

    fn kind(&self) -> &'static str {
        "memory"
    }
}

/// 跨文件工作区：只索引 + 按偏移读，节点数据不常驻；支持「同编号多文件」取并集。
pub struct LazyWorkspace {
    backend: Box<dyn Backend>,
}

impl LazyWorkspace {
    /// 按当前索引模式打开。工作区模式下若索引缺失或不覆盖给定文件，回退到整份载入。
    pub fn from_paths(paths: &[String]) -> Result<Self, String> {
        let backend: Box<dyn Backend> = match index_mode() {
            IndexMode::Sidecar => Box::new(SidecarBackend::open(paths)?),
            IndexMode::Workspace => {
                let ws_root = crate::wsidx::workspace_root(Path::new(&paths[0]));
                let dir = crate::wsidx::index_dir(&ws_root);
                let mut covered = dir.exists();
                if covered {
                    let r = crate::wsidx::Reader::open(&ws_root)?;
                    covered = paths.iter().all(|p| r.covers(p));
                }
                if covered {
                    Box::new(WorkspaceBackend::open(&ws_root)?)
                } else {
                    Box::new(MemoryBackend::load(paths)?)
                }
            }
        };
        Ok(Self { backend })
    }

    /// 当前后端名字：workspace / sidecar / memory（回退时是 memory）。
    pub fn backend_kind(&self) -> &'static str {
        self.backend.kind()
    }

    /// 打开过的索引 / 数据文件次数。
    pub fn opens(&self) -> u64 {
        self.backend.opens()
    }

    /// 台账里的文件数（memory 后端等于载入的文件数）。
    pub fn file_count(&self) -> usize {
        self.backend.file_count()
    }

    /// 按 UUID 定位目标节点（懒加载，只读那一段）。
    pub fn find(&mut self, id: Uuid) -> Option<(String, Node)> {
        self.backend.locate(id).into_iter().next()
    }

    /// 该编号在**每个相关文件**里的一份（名字/值 + 来源文件），按文件顺序。
    /// 「同编号多文件」是正常现象，所以这里是「多份」而不是「一份」。
    pub fn node_views(&mut self, id: Uuid) -> Vec<(String, Node)> {
        self.backend.locate(id)
    }

    /// 并集后的孩子：所有文件里父边指向该编号的节点，同一个孩子编号只算一次。
    pub fn children_union(&mut self, id: Uuid) -> Vec<(String, Node)> {
        self.backend.children(id)
    }

    /// 谁引用了我：跨所有文件聚合反向边，逐个懒加载源节点。
    pub fn references_to(&mut self, id: Uuid) -> Vec<(String, Node)> {
        let srcs = self.backend.references(id);
        let mut out = Vec::new();
        for (_file, s) in srcs {
            if let Some((path, n)) = self.find(s) {
                out.push((path, n));
            }
        }
        out
    }

    /// 按树根取整棵子树（含根）。
    pub fn subtree(&mut self, root: Uuid) -> Vec<(String, Node)> {
        self.backend.subtree(root)
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
