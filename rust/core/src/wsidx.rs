//! 工作区统一索引（wsidx-v1）：三本台账 + 追加日志 + 显式压实。
//!
//! - 定位本（loc）：编号 → 文件 + 偏移 + 长度 + 所属树根，按编号排序
//! - 关系本（rel）：树根 + 父 → 孩子 + 位置，按「树根 + 父」排序（同一棵树连续存放）
//! - 反向本（rev）：被引用编号 → 来源编号 + 文件，按被引用编号排序
//!
//! 每本 = `index.manifest` 里的块表 + `loc|rel|rev-NNNN.blk`（有序块，每块 100 万条）
//! + `loc|rel|rev.log`（追加日志）。写入只追加日志；`compact` 把日志合并回块。
//! 每个文件在台账里带指纹（尺寸 + 修改时间），对不上就作废并回退到整份载入。
//! **未知记录类型一律按长度跳过**，以后加段不需要升主版本。

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};

use crate::codec::Uuid;
use crate::index;

pub const MAGIC: &[u8; 5] = b"XWSIX";
pub const VERSION: u8 = 1;
pub const INDEX_DIR: &str = ".xirang-index";
/// 每块的条目上限（36–68 MB，视台账而定）。
pub const BLOCK_ENTRIES: usize = 1_000_000;
/// 日志超过主干这个比例时，`status` 提示压实。
pub const COMPACT_HINT_RATIO: f64 = 0.30;

const KIND_LOC: u8 = 1;
const KIND_REL: u8 = 2;
const KIND_REV: u8 = 3;
const PART_BLOCK: u8 = 1;
const PART_LOG: u8 = 2;
const PART_MANIFEST: u8 = 3;

const REC_ENTRY: u8 = 1;
const REC_FILE: u8 = 3;
const REC_FILE_REMOVE: u8 = 4;

pub const LOC_ENTRY: usize = 52; // uuid16 + file_id4 + off8 + len8 + root16
pub const REL_ENTRY: usize = 68; // root16 + parent16 + child16 + file_id4 + off8 + len8
pub const REV_ENTRY: usize = 36; // target16 + source16 + file_id4

const HEADER: &str = "\
XiRang workspace index (XWSIX) v1
Three ledgers per workspace: loc (id -> file+offset+root), rel (root+parent -> child), rev (target -> source).
Each ledger = block table in index.manifest + sorted blocks (<=1M entries) + append-only log.
Unknown record types must be skipped by length; fingerprints (size+mtime) decide staleness.";

// ---------------------------------------------------------------- 条目

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocEntry {
    pub uuid: Uuid,
    pub file_id: u32,
    pub off: u64,
    pub len: u64,
    pub root: Uuid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RelEntry {
    pub root: Uuid,
    pub parent: Uuid,
    pub child: Uuid,
    pub file_id: u32,
    pub off: u64,
    pub len: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RevEntry {
    pub target: Uuid,
    pub source: Uuid,
    pub file_id: u32,
}

fn enc_loc(e: &LocEntry, o: &mut Vec<u8>) {
    o.extend_from_slice(&e.uuid.0);
    o.extend_from_slice(&e.file_id.to_be_bytes());
    o.extend_from_slice(&e.off.to_be_bytes());
    o.extend_from_slice(&e.len.to_be_bytes());
    o.extend_from_slice(&e.root.0);
}

fn dec_loc(b: &[u8]) -> LocEntry {
    LocEntry {
        uuid: Uuid(b[0..16].try_into().unwrap()),
        file_id: u32::from_be_bytes(b[16..20].try_into().unwrap()),
        off: u64::from_be_bytes(b[20..28].try_into().unwrap()),
        len: u64::from_be_bytes(b[28..36].try_into().unwrap()),
        root: Uuid(b[36..52].try_into().unwrap()),
    }
}

fn enc_rel(e: &RelEntry, o: &mut Vec<u8>) {
    o.extend_from_slice(&e.root.0);
    o.extend_from_slice(&e.parent.0);
    o.extend_from_slice(&e.child.0);
    o.extend_from_slice(&e.file_id.to_be_bytes());
    o.extend_from_slice(&e.off.to_be_bytes());
    o.extend_from_slice(&e.len.to_be_bytes());
}

fn dec_rel(b: &[u8]) -> RelEntry {
    RelEntry {
        root: Uuid(b[0..16].try_into().unwrap()),
        parent: Uuid(b[16..32].try_into().unwrap()),
        child: Uuid(b[32..48].try_into().unwrap()),
        file_id: u32::from_be_bytes(b[48..52].try_into().unwrap()),
        off: u64::from_be_bytes(b[52..60].try_into().unwrap()),
        len: u64::from_be_bytes(b[60..68].try_into().unwrap()),
    }
}

fn enc_rev(e: &RevEntry, o: &mut Vec<u8>) {
    o.extend_from_slice(&e.target.0);
    o.extend_from_slice(&e.source.0);
    o.extend_from_slice(&e.file_id.to_be_bytes());
}

fn dec_rev(b: &[u8]) -> RevEntry {
    RevEntry {
        target: Uuid(b[0..16].try_into().unwrap()),
        source: Uuid(b[16..32].try_into().unwrap()),
        file_id: u32::from_be_bytes(b[32..36].try_into().unwrap()),
    }
}

/// 排序键：定位/反向本用编号；关系本用「树根 + 父」。
fn key_loc(e: &LocEntry) -> [u8; 32] {
    let mut k = [0u8; 32];
    k[..16].copy_from_slice(&e.uuid.0);
    k
}
fn key_rev(e: &RevEntry) -> [u8; 32] {
    let mut k = [0u8; 32];
    k[..16].copy_from_slice(&e.target.0);
    k
}
fn key_rel(e: &RelEntry) -> [u8; 32] {
    let mut k = [0u8; 32];
    k[..16].copy_from_slice(&e.root.0);
    k[16..].copy_from_slice(&e.parent.0);
    k
}
fn key_uuid(id: Uuid) -> [u8; 32] {
    let mut k = [0u8; 32];
    k[..16].copy_from_slice(&id.0);
    k
}

// ---------------------------------------------------------------- manifest

#[derive(Clone, Debug, Default)]
pub struct FileEntry {
    pub id: u32,
    pub path: String,
    pub size: u64,
    pub mtime_sec: i64,
    pub mtime_nsec: u32,
    pub uuid_count: u64,
    /// 块里那条目的代号（压实时刻）；与 `cur_gen` 不等说明块已过期。
    pub gen: u64,
    /// 最新代号（每次追加都会前进）。
    pub cur_gen: u64,
}

#[derive(Clone, Debug, Default)]
pub struct BlockInfo {
    pub index: u32,
    pub first: [u8; 32],
    pub last: [u8; 32],
    pub count: u64,
    pub name: String,
}

#[derive(Clone, Debug, Default)]
pub struct LedgerInfo {
    pub blocks: Vec<BlockInfo>,
    pub log_bytes: u64,
    /// 主干（所有块）的字节总量——`maintenance_needed` 用它做 O(1) 判断。
    pub base_bytes: u64,
}

#[derive(Clone, Debug, Default)]
pub struct Manifest {
    pub generation: u64,
    pub next_file_id: u32,
    pub files: Vec<FileEntry>,
    pub loc: LedgerInfo,
    pub rel: LedgerInfo,
    pub rev: LedgerInfo,
}

fn ledger_mut(m: &mut Manifest, kind: u8) -> &mut LedgerInfo {
    match kind {
        KIND_LOC => &mut m.loc,
        KIND_REL => &mut m.rel,
        _ => &mut m.rev,
    }
}

fn ledger_ref(m: &Manifest, kind: u8) -> &LedgerInfo {
    match kind {
        KIND_LOC => &m.loc,
        KIND_REL => &m.rel,
        _ => &m.rev,
    }
}

fn entry_size(kind: u8) -> usize {
    match kind {
        KIND_LOC => LOC_ENTRY,
        KIND_REL => REL_ENTRY,
        _ => REV_ENTRY,
    }
}

// ---------------------------------------------------------------- 路径与底层读写

/// 工作区根：文件所在目录（传目录则用目录本身）；`XIRANG_WORKSPACE` 可覆盖。
pub fn workspace_root(path: &Path) -> PathBuf {
    if let Some(ws) = std::env::var_os("XIRANG_WORKSPACE") {
        if !ws.is_empty() {
            return PathBuf::from(ws);
        }
    }
    let abs = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if abs.is_dir() {
        abs
    } else {
        abs.parent().map(|p| p.to_path_buf()).unwrap_or(abs)
    }
}

pub fn index_dir(ws_root: &Path) -> PathBuf {
    ws_root.join(INDEX_DIR)
}

/// 词法绝对路径：不做 canonicalize（那是几十微秒级的系统调用，几百个文件就吃掉几十毫秒），
/// 只把相对路径接上当前目录、折叠 `.` 与 `..`。用于「命令行给的路径在不在台账里」的比对。
pub fn lexical_abs(raw: &str) -> String {
    use std::sync::OnceLock;
    static CWD: OnceLock<PathBuf> = OnceLock::new();
    let p = Path::new(raw);
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        CWD.get_or_init(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
            .join(p)
    };
    normalize(&joined).to_string_lossy().into_owned()
}

fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn ledger_name(kind: u8) -> &'static str {
    match kind {
        KIND_LOC => "loc",
        KIND_REL => "rel",
        _ => "rev",
    }
}

fn manifest_path(dir: &Path) -> PathBuf {
    dir.join("index.manifest")
}

fn log_path(dir: &Path, kind: u8) -> PathBuf {
    dir.join(format!("{}.log", ledger_name(kind)))
}

/// 块文件名带「代数」前缀：压实写新代数的块，换完 manifest 才删旧代数。
/// 这样中断在任何时刻都不会出现「manifest 指着一半新一半旧的块」。
fn block_path(dir: &Path, kind: u8, generation: u64, index_v: u32) -> PathBuf {
    dir.join(format!("{}-g{generation}-{index_v:04}.blk", ledger_name(kind)))
}

fn lock_path(dir: &Path) -> PathBuf {
    dir.join("lock")
}

fn write_prefix(o: &mut Vec<u8>, kind: u8, part: u8) {
    let header = HEADER.as_bytes();
    o.extend_from_slice(MAGIC);
    o.push(VERSION);
    o.push(kind);
    o.push(part);
    o.extend_from_slice(&(header.len() as u32).to_be_bytes());
    o.extend_from_slice(header);
}

const PREFIX_LEN: usize = 5 + 3 + 4; // 固定部分；头文本长度另算

fn fixed_prefix_len() -> usize {
    PREFIX_LEN + HEADER.as_bytes().len()
}

fn read_prefix(f: &mut File, want_kind: u8, want_part: u8) -> Result<(), String> {
    let mut head = vec![0u8; fixed_prefix_len()];
    f.read_exact(&mut head).map_err(|_| "F013：索引文件损坏（读不到文件头）".to_string())?;
    if &head[..5] != MAGIC {
        return Err("F013：索引文件损坏（魔数不符）".into());
    }
    if head[5] != VERSION {
        return Err("F013：索引版本不支持".into());
    }
    if head[6] != want_kind || head[7] != want_part {
        return Err("F013：索引文件类型不符".into());
    }
    Ok(())
}

fn write_file_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)
}

fn rd_u32(b: &[u8], off: &mut usize) -> Result<u32, String> {
    if *off + 4 > b.len() {
        return Err("F013：索引文件损坏（越界）".into());
    }
    let v = u32::from_be_bytes(b[*off..*off + 4].try_into().unwrap());
    *off += 4;
    Ok(v)
}

fn rd_u64(b: &[u8], off: &mut usize) -> Result<u64, String> {
    if *off + 8 > b.len() {
        return Err("F013：索引文件损坏（越界）".into());
    }
    let v = u64::from_be_bytes(b[*off..*off + 8].try_into().unwrap());
    *off += 8;
    Ok(v)
}

fn rd_i64(b: &[u8], off: &mut usize) -> Result<i64, String> {
    if *off + 8 > b.len() {
        return Err("F013：索引文件损坏（越界）".into());
    }
    let v = i64::from_be_bytes(b[*off..*off + 8].try_into().unwrap());
    *off += 8;
    Ok(v)
}

fn rd_key(b: &[u8], off: &mut usize) -> Result<[u8; 32], String> {
    if *off + 32 > b.len() {
        return Err("F013：索引文件损坏（边界越界）".into());
    }
    let mut k = [0u8; 32];
    k.copy_from_slice(&b[*off..*off + 32]);
    *off += 32;
    Ok(k)
}

fn rd_file_rec(b: &[u8], off: &mut usize) -> Result<FileEntry, String> {
    let id = rd_u32(b, off)?;
    let plen = rd_u32(b, off)? as usize;
    if *off + plen > b.len() {
        return Err("F013：索引文件损坏（文件表越界）".into());
    }
    let path = String::from_utf8_lossy(&b[*off..*off + plen]).into_owned();
    *off += plen;
    Ok(FileEntry {
        id,
        path,
        size: rd_u64(b, off)?,
        mtime_sec: rd_i64(b, off)?,
        mtime_nsec: rd_u32(b, off)?,
        uuid_count: rd_u64(b, off)?,
        gen: rd_u64(b, off)?,
        cur_gen: rd_u64(b, off)?,
    })
}

fn enc_file_rec(f: &FileEntry, o: &mut Vec<u8>) {
    o.extend_from_slice(&f.id.to_be_bytes());
    o.extend_from_slice(&(f.path.as_bytes().len() as u32).to_be_bytes());
    o.extend_from_slice(f.path.as_bytes());
    o.extend_from_slice(&f.size.to_be_bytes());
    o.extend_from_slice(&f.mtime_sec.to_be_bytes());
    o.extend_from_slice(&f.mtime_nsec.to_be_bytes());
    o.extend_from_slice(&f.uuid_count.to_be_bytes());
    o.extend_from_slice(&f.gen.to_be_bytes());
    o.extend_from_slice(&f.cur_gen.to_be_bytes());
}

fn enc_manifest(m: &Manifest) -> Vec<u8> {
    let mut o = Vec::new();
    write_prefix(&mut o, 0, PART_MANIFEST);
    o.extend_from_slice(&m.generation.to_be_bytes());
    o.extend_from_slice(&m.next_file_id.to_be_bytes());
    o.extend_from_slice(&(m.files.len() as u64).to_be_bytes());
    for f in &m.files {
        enc_file_rec(f, &mut o);
    }
    for kind in [KIND_LOC, KIND_REL, KIND_REV] {
        let l = ledger_ref(m, kind);
        o.extend_from_slice(&(l.blocks.len() as u64).to_be_bytes());
        for b in &l.blocks {
            o.extend_from_slice(&b.index.to_be_bytes());
            o.extend_from_slice(&b.first);
            o.extend_from_slice(&b.last);
            o.extend_from_slice(&b.count.to_be_bytes());
            o.extend_from_slice(&(b.name.as_bytes().len() as u32).to_be_bytes());
            o.extend_from_slice(b.name.as_bytes());
        }
        o.extend_from_slice(&l.log_bytes.to_be_bytes());
        o.extend_from_slice(&l.base_bytes.to_be_bytes());
    }
    o
}

fn dec_manifest(b: &[u8]) -> Result<Manifest, String> {
    let mut off = fixed_prefix_len();
    let mut m = Manifest::default();
    m.generation = rd_u64(b, &mut off)?;
    m.next_file_id = rd_u32(b, &mut off)?;
    let fcount = rd_u64(b, &mut off)?;
    for _ in 0..fcount {
        m.files.push(rd_file_rec(b, &mut off)?);
    }
    for kind in [KIND_LOC, KIND_REL, KIND_REV] {
        let bcount = rd_u64(b, &mut off)?;
        let mut blocks = Vec::new();
        for _ in 0..bcount {
            let index_v = rd_u32(b, &mut off)?;
            let first = rd_key(b, &mut off)?;
            let last = rd_key(b, &mut off)?;
            let count = rd_u64(b, &mut off)?;
            let nlen = rd_u32(b, &mut off)? as usize;
            if off + nlen > b.len() {
                return Err("F013：索引文件损坏（块表越界）".into());
            }
            let name = String::from_utf8_lossy(&b[off..off + nlen]).into_owned();
            off += nlen;
            blocks.push(BlockInfo { index: index_v, first, last, count, name });
        }
        let log_bytes = rd_u64(b, &mut off)?;
        let base_bytes = rd_u64(b, &mut off)?;
        *ledger_mut(&mut m, kind) = LedgerInfo { blocks, log_bytes, base_bytes };
    }
    Ok(m)
}

fn read_manifest(dir: &Path) -> Result<Manifest, String> {
    let path = manifest_path(dir);
    if !path.exists() {
        return Ok(Manifest::default());
    }
    let bytes = fs::read(&path).map_err(|e| e.to_string())?;
    dec_manifest(&bytes)
}

// ---------------------------------------------------------------- 写者锁

pub struct LockGuard {
    path: PathBuf,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// 取写者锁：已有锁且进程仍活着 → F014；进程已死视为陈旧锁，自动回收。
pub fn lock_writer(dir: &Path) -> Result<LockGuard, String> {
    fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let path = lock_path(dir);
    match OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(mut f) => {
            let _ = write!(f, "{}", std::process::id());
            Ok(LockGuard { path })
        }
        Err(_) => {
            let alive = fs::read_to_string(&path)
                .ok()
                .and_then(|s| s.trim().parse::<i32>().ok())
                .map(|pid| unsafe { libc::kill(pid, 0) } == 0)
                .unwrap_or(false);
            if alive {
                return Err("F014：索引正被另一个进程写入（锁被占用）".into());
            }
            let _ = fs::remove_file(&path);
            lock_writer(dir)
        }
    }
}

// ---------------------------------------------------------------- 扫描数据文件

#[derive(Clone, Debug, Default)]
pub struct FileEntries {
    pub loc: Vec<LocEntry>,
    pub rel: Vec<RelEntry>,
    pub rev: Vec<RevEntry>,
}

impl FileEntries {
    fn bytes(&self, kind: u8) -> Vec<Vec<u8>> {
        match kind {
            KIND_LOC => self
                .loc
                .iter()
                .map(|e| {
                    let mut b = Vec::with_capacity(LOC_ENTRY);
                    enc_loc(e, &mut b);
                    b
                })
                .collect(),
            KIND_REL => self
                .rel
                .iter()
                .map(|e| {
                    let mut b = Vec::with_capacity(REL_ENTRY);
                    enc_rel(e, &mut b);
                    b
                })
                .collect(),
            _ => self
                .rev
                .iter()
                .map(|e| {
                    let mut b = Vec::with_capacity(REV_ENTRY);
                    enc_rev(e, &mut b);
                    b
                })
                .collect(),
        }
    }

    pub fn len(&self) -> usize {
        self.loc.len() + self.rel.len() + self.rev.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// 文件指纹：尺寸 + 修改时间（秒 / 纳秒）。
pub fn fingerprint_of(path: &Path) -> Option<(u64, i64, u32)> {
    let meta = fs::metadata(path).ok()?;
    let d = meta.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some((meta.len(), d.as_secs() as i64, d.subsec_nanos()))
}

/// 扫一个 `.xirang` 文件，产出它在三本台账里的全部条目（偏移是数据段内的相对偏移）。
pub fn scan_file(path: &Path, file_id: u32) -> Result<FileEntries, String> {
    let data = fs::read(path).map_err(|e| e.to_string())?;
    let idx = index::build(&data)?;

    let mut loc_off: HashMap<Uuid, (u64, u64)> = HashMap::new();
    for (id, (off, len)) in &idx.roots {
        loc_off.insert(*id, (*off, *len));
    }
    for cs in idx.children.values() {
        for (child, off, len) in cs {
            loc_off.insert(*child, (*off, *len));
        }
    }

    let mut loc: Vec<LocEntry> = idx
        .assign
        .iter()
        .map(|(uuid, root)| {
            let (off, len) = loc_off.get(uuid).copied().unwrap_or((0, 0));
            LocEntry { uuid: *uuid, file_id, off, len, root: *root }
        })
        .collect();
    loc.sort_by(|a, b| key_loc(a).cmp(&key_loc(b)).then_with(|| a.file_id.cmp(&b.file_id)));

    let mut rel: Vec<RelEntry> = Vec::new();
    for (parent, cs) in &idx.children {
        let root = idx.assign.get(parent).copied().unwrap_or(*parent);
        for (child, off, len) in cs {
            rel.push(RelEntry {
                root,
                parent: *parent,
                child: *child,
                file_id,
                off: *off,
                len: *len,
            });
        }
    }
    rel.sort_by(|a, b| key_rel(a).cmp(&key_rel(b)).then_with(|| a.child.0.cmp(&b.child.0)));

    let mut rev: Vec<RevEntry> = Vec::new();
    for (target, sources) in &idx.reverse {
        for s in sources {
            rev.push(RevEntry { target: *target, source: *s, file_id });
        }
    }
    rev.sort_by(|a, b| key_rev(a).cmp(&key_rev(b)).then_with(|| a.source.0.cmp(&b.source.0)));

    Ok(FileEntries { loc, rel, rev })
}

/// 收集工作区里的数据文件（跳过索引目录）。
pub fn collect_data_files(ws_root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    collect_inner(ws_root, &mut out);
    out.sort();
    out
}

fn collect_inner(dir: &Path, out: &mut Vec<PathBuf>) {
    let rd = match fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return,
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            if p.file_name().map(|n| n == INDEX_DIR).unwrap_or(false) {
                continue;
            }
            collect_inner(&p, out);
        } else if p.extension().and_then(|s| s.to_str()) == Some("xirang") {
            out.push(p);
        }
    }
}

// ---------------------------------------------------------------- 写：块与日志

fn write_blocks(
    dir: &Path,
    kind: u8,
    generation: u64,
    entries: &[Vec<u8>],
) -> Result<LedgerInfo, String> {
    // 新块写新名字（带代数），旧块由调用方在 manifest 切换后删除
    let mut blocks = Vec::new();
    let mut base_bytes = 0u64;
    for (i, chunk) in entries.chunks(BLOCK_ENTRIES).enumerate() {
        let mut o = Vec::new();
        write_prefix(&mut o, kind, PART_BLOCK);
        o.extend_from_slice(&(chunk.len() as u64).to_be_bytes());
        for e in chunk {
            o.extend_from_slice(e);
        }
        let mut first = [0u8; 32];
        let mut last = [0u8; 32];
        if let Some(e) = chunk.first() {
            first.copy_from_slice(&e[..32]);
        }
        if let Some(e) = chunk.last() {
            last.copy_from_slice(&e[..32]);
        }
        let path = block_path(dir, kind, generation, i as u32);
        write_file_atomic(&path, &o).map_err(|e| e.to_string())?;
        base_bytes += o.len() as u64;
        blocks.push(BlockInfo {
            index: i as u32,
            first,
            last,
            count: chunk.len() as u64,
            name: path.file_name().unwrap().to_string_lossy().into_owned(),
        });
    }
    ensure_log(dir, kind)?;
    Ok(LedgerInfo { blocks, log_bytes: 0, base_bytes })
}

fn ensure_log(dir: &Path, kind: u8) -> Result<(), String> {
    let path = log_path(dir, kind);
    if path.exists() {
        return Ok(());
    }
    let mut o = Vec::new();
    write_prefix(&mut o, kind, PART_LOG);
    write_file_atomic(&path, &o).map_err(|e| e.to_string())?;
    Ok(())
}

fn append_rec(f: &mut File, kind: u8, payload: &[u8]) -> std::io::Result<u64> {
    let mut o = Vec::with_capacity(payload.len() + 5);
    o.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    o.push(kind);
    o.extend_from_slice(payload);
    f.write_all(&o)?;
    Ok(o.len() as u64)
}

/// 同样的记录格式，但攒进缓冲区（供「一次 write」的批量追加用）。
fn push_rec(out: &mut Vec<u8>, kind: u8, payload: &[u8]) {
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.push(kind);
    out.extend_from_slice(payload);
}

/// 删掉某一本里「不属于当前 manifest」的块（换完 manifest 之后调用）。
fn remove_other_blocks(dir: &Path, kind: u8, keep: &LedgerInfo) {
    let prefix = format!("{}-", ledger_name(kind));
    let keep_names: HashSet<&str> = keep.blocks.iter().map(|b| b.name.as_str()).collect();
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if n.starts_with(&prefix) && n.ends_with(".blk") && !keep_names.contains(n.as_str()) {
                let _ = fs::remove_file(e.path());
            }
        }
    }
}

// ---------------------------------------------------------------- 对外：重建 / 追加 / 压实 / 维护

#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub files: usize,
    pub loc: u64,
    pub rel: u64,
    pub rev: u64,
    pub blocks: usize,
    pub bytes_written: u64,
    pub log_bytes: u64,
}

/// 全量重建：扫描给定文件（缺省 = 工作区全部 `.xirang`），重写块与 manifest。
pub fn rebuild(ws_root: &Path, files: &[PathBuf]) -> Result<Stats, String> {
    let dir = index_dir(ws_root);
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let _guard = lock_writer(&dir)?;
    // 给目录就展开成目录下的 `.xirang`；给文件就用文件；什么都不给就扫整个工作区
    let mut targets: Vec<PathBuf> = Vec::new();
    for p in files {
        if p.is_dir() {
            targets.extend(collect_data_files(p));
        } else {
            targets.push(p.clone());
        }
    }
    if targets.is_empty() && files.is_empty() {
        targets = collect_data_files(ws_root);
    }
    let generation = read_manifest(&dir).map(|m| m.generation + 1).unwrap_or(1);
    let (prepared, changed) = build_ledgers(&dir, &targets, generation, true)?;
    if !changed.is_empty() {
        eprintln!("（提示：{} 个文件在重建期间被改动，它们的索引本次标记为待重建）", changed.len());
    }
    prepared.map(|(_, s)| s).ok_or_else(|| "重建未完成".to_string())
}

/// 无锁地重扫目标文件、重写三本块与 manifest（日志由调用方处理）。
///
/// 返回 `(备好的 manifest + 统计, 扫描期间被改动的文件)`。指纹一律取「扫描前」的值，
/// 所以即使扫描期间文件被改，读者的指纹校验也会把它判为过期 → 回退，不会读到错数据。
/// `commit` = false 时只写出新块与 manifest 内容，不落地、不删旧块（交给压实决定）。
fn build_ledgers(
    dir: &Path,
    targets: &[PathBuf],
    generation: u64,
    commit: bool,
) -> Result<(Option<(Manifest, Stats)>, Vec<String>), String> {
    let mut m = Manifest { generation, next_file_id: 1, ..Default::default() };
    let mut loc: Vec<Vec<u8>> = Vec::new();
    let mut rel: Vec<Vec<u8>> = Vec::new();
    let mut rev: Vec<Vec<u8>> = Vec::new();
    let mut counts = (0u64, 0u64, 0u64);
    let mut changed: Vec<String> = Vec::new();
    for p in targets {
        let abs = p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
        // 指纹取扫描前的值：万一扫描期间被改，读者会因指纹不符而回退（安全）
        let before = match fingerprint_of(&abs) {
            Some(f) => f,
            None => continue,
        };
        let id = m.next_file_id;
        m.next_file_id += 1;
        let e = scan_file(&abs, id)?;
        if fingerprint_of(&abs) != Some(before) {
            changed.push(abs.to_string_lossy().into_owned());
        }
        let (size, sec, nsec) = before;
        counts.0 += e.loc.len() as u64;
        counts.1 += e.rel.len() as u64;
        counts.2 += e.rev.len() as u64;
        m.files.push(FileEntry {
            id,
            path: abs.to_string_lossy().into_owned(),
            size,
            mtime_sec: sec,
            mtime_nsec: nsec,
            uuid_count: e.loc.len() as u64,
            gen: m.generation,
            cur_gen: m.generation,
        });
        loc.extend(e.bytes(KIND_LOC));
        rel.extend(e.bytes(KIND_REL));
        rev.extend(e.bytes(KIND_REV));
    }
    // 台账是全局有序的：合并多个文件后必须重新排序（否则二分查找会漏条目）
    loc.sort();
    rel.sort();
    rev.sort();
    let mut blocks = 0usize;
    let mut infos: Vec<(u8, LedgerInfo)> = Vec::new();
    for (kind, es) in [(KIND_LOC, &loc), (KIND_REL, &rel), (KIND_REV, &rev)] {
        let info = write_blocks(dir, kind, generation, es)?;
        blocks += info.blocks.len();
        infos.push((kind, info));
    }
    for (kind, info) in infos {
        let lp = log_path(dir, kind);
        let log_bytes = fs::metadata(&lp).map(|x| x.len()).unwrap_or(0);
        let l = ledger_mut(&mut m, kind);
        l.blocks = info.blocks;
        l.base_bytes = info.base_bytes;
        l.log_bytes = log_bytes;
    }
    let stats = Stats {
        files: m.files.len(),
        loc: counts.0,
        rel: counts.1,
        rev: counts.2,
        blocks,
        bytes_written: 0,
        log_bytes: m.loc.log_bytes + m.rel.log_bytes + m.rev.log_bytes,
    };
    if commit {
        commit_manifest(dir, &m)?;
    }
    Ok((Some((m, stats)), changed))
}

/// 落地 manifest 并清掉不属于它的旧块（调用方负责持锁）。
fn commit_manifest(dir: &Path, m: &Manifest) -> Result<(), String> {
    write_file_atomic(&manifest_path(dir), &enc_manifest(m)).map_err(|e| e.to_string())?;
    for kind in [KIND_LOC, KIND_REL, KIND_REV] {
        remove_other_blocks(dir, kind, ledger_ref(m, kind));
    }
    Ok(())
}

fn discard_blocks(dir: &Path, m: &Manifest) {
    for kind in [KIND_LOC, KIND_REL, KIND_REV] {
        for b in &ledger_ref(m, kind).blocks {
            let _ = fs::remove_file(dir.join(&b.name));
        }
    }
}

/// 一个数据文件写完后调用：重扫该文件，追加「新一代」记录到三本日志。
/// 旧代号的条目会自动作废（文件被整份重写时，所有偏移都变了）。
pub fn append_file(ws_root: &Path, data_path: &Path) -> Result<Stats, String> {
    let dir = index_dir(ws_root);
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let _guard = lock_writer(&dir)?;
    append_file_locked(&dir, data_path)
}

fn append_file_locked(dir: &Path, data_path: &Path) -> Result<Stats, String> {
    let abs = data_path
        .canonicalize()
        .unwrap_or_else(|_| data_path.to_path_buf())
        .to_string_lossy()
        .into_owned();
    let (size, sec, nsec) =
        fingerprint_of(Path::new(&abs)).ok_or_else(|| "文件不存在".to_string())?;
    let mut m = read_manifest(dir)?;
    let file_id = match m.files.iter().find(|f| f.path == abs) {
        Some(f) => f.id,
        None => {
            let id = m.next_file_id.max(1);
            m.next_file_id = id + 1;
            m.files.push(FileEntry {
                id,
                path: abs.clone(),
                size,
                mtime_sec: sec,
                mtime_nsec: nsec,
                uuid_count: 0,
                gen: 0,
                cur_gen: 0,
            });
            id
        }
    };
    let entries = scan_file(Path::new(&abs), file_id)?;
    let new_gen = m.generation + 1;
    let rec = FileEntry {
        id: file_id,
        path: abs,
        size,
        mtime_sec: sec,
        mtime_nsec: nsec,
        uuid_count: entries.loc.len() as u64,
        gen: m.files.iter().find(|f| f.id == file_id).map(|f| f.gen).unwrap_or(0),
        cur_gen: new_gen,
    };
    let mut rec_bytes = Vec::new();
    enc_file_rec(&rec, &mut rec_bytes);

    let mut written = 0u64;
    for kind in [KIND_LOC, KIND_REL, KIND_REV] {
        ensure_log(dir, kind)?;
        // 攒成一个缓冲区、一次 write：逐条 append_rec 在 347 万节点上是约 700 万次系统调用
        // （实测 18 秒里有 15 秒花在这里），批量写之后降到几秒以内。
        let mut buf: Vec<u8> = Vec::new();
        push_rec(&mut buf, REC_FILE, &rec_bytes);
        for b in entries.bytes(kind) {
            push_rec(&mut buf, REC_ENTRY, &b);
        }
        let mut f = OpenOptions::new()
            .append(true)
            .open(log_path(dir, kind))
            .map_err(|e| e.to_string())?;
        f.write_all(&buf).map_err(|e| e.to_string())?;
        written += buf.len() as u64;
    }
    // 日志先落，再更新 manifest：中途崩了也能靠日志里的文件记录恢复（更安全的方向）
    m.generation = new_gen;
    if let Some(f) = m.files.iter_mut().find(|f| f.id == file_id) {
        *f = rec;
    }
    for kind in [KIND_LOC, KIND_REL, KIND_REV] {
        let bytes = fs::metadata(log_path(dir, kind)).map(|x| x.len()).unwrap_or(0);
        ledger_mut(&mut m, kind).log_bytes = bytes;
    }
    write_file_atomic(&manifest_path(dir), &enc_manifest(&m)).map_err(|e| e.to_string())?;
    Ok(Stats {
        files: m.files.len(),
        loc: entries.loc.len() as u64,
        rel: entries.rel.len() as u64,
        rev: entries.rev.len() as u64,
        blocks: 0,
        bytes_written: written,
        log_bytes: m.loc.log_bytes + m.rel.log_bytes + m.rev.log_bytes,
    })
}

/// 压实：**扫描不持锁**（写者照常写），只在最后「换 manifest + 清日志」的瞬间持锁；
/// 扫描期间若有文件被改动，本轮整体放弃、下次再来（绝不写出半新半旧的状态）。
pub fn compact(ws_root: &Path) -> Result<Stats, String> {
    let dir = index_dir(ws_root);
    let m = read_manifest(&dir)?;
    let targets: Vec<PathBuf> = m
        .files
        .iter()
        .map(|f| PathBuf::from(&f.path))
        .filter(|p| p.exists())
        .collect();
    let generation = m.generation + 1;
    let (prepared, changed) = build_ledgers(&dir, &targets, generation, false)?;
    let Some((mut prepared, stats)) = prepared else {
        return Err(format!(
            "本次跳过压实：{} 个文件在扫描期间被改动，稍后重试",
            changed.len()
        ));
    };
    if !changed.is_empty() {
        discard_blocks(&dir, &prepared);
        return Err(format!(
            "本次跳过压实：{} 个文件在扫描期间被改动，稍后重试",
            changed.len()
        ));
    }
    // 拿锁这一段只有毫秒级：再核一次指纹，然后落 manifest、清日志
    let _guard = lock_writer(&dir)?;
    for f in &prepared.files {
        if fingerprint_of(Path::new(&f.path))
            != Some((f.size, f.mtime_sec, f.mtime_nsec))
        {
            discard_blocks(&dir, &prepared);
            return Err("本次跳过压实：扫描结束后又有文件被改动，稍后重试".into());
        }
    }
    for kind in [KIND_LOC, KIND_REL, KIND_REV] {
        let mut o = Vec::new();
        write_prefix(&mut o, kind, PART_LOG);
        ledger_mut(&mut prepared, kind).log_bytes = o.len() as u64;
        write_file_atomic(&log_path(&dir, kind), &o).map_err(|e| e.to_string())?;
    }
    commit_manifest(&dir, &prepared)?;
    Ok(stats)
}

/// 自动整理开关（`XIRANG_INDEX_MAINTENANCE=off` 关闭，默认开）。
pub fn maintenance_enabled() -> bool {
    !matches!(
        std::env::var("XIRANG_INDEX_MAINTENANCE").ok().as_deref(),
        Some("off") | Some("0") | Some("false") | Some("no")
    )
}

fn compact_ratio() -> f64 {
    std::env::var("XIRANG_INDEX_COMPACT_RATIO")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(COMPACT_HINT_RATIO)
}

fn compact_min_bytes() -> u64 {
    std::env::var("XIRANG_INDEX_COMPACT_MIN_BYTES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1_000_000)
}

/// O(1) 判断要不要整理：只读 manifest（每本台账的主干 / 日志字节数都在里面）。
/// 返回需要整理时日志的字节数。
pub fn maintenance_needed(ws_root: &Path) -> Option<u64> {
    let m = read_manifest(&index_dir(ws_root)).ok()?;
    let base = m.loc.base_bytes + m.rel.base_bytes + m.rev.base_bytes;
    let log = m.loc.log_bytes + m.rel.log_bytes + m.rev.log_bytes;
    if base >= compact_min_bytes() && (log as f64) > (base as f64) * compact_ratio() {
        Some(log)
    } else {
        None
    }
}

/// 清理工作区里已不存在的文件条目。
pub fn gc(ws_root: &Path) -> Result<usize, String> {
    let dir = index_dir(ws_root);
    let _guard = lock_writer(&dir)?;
    let mut m = read_manifest(&dir)?;
    let removed: Vec<FileEntry> =
        m.files.iter().filter(|f| !Path::new(&f.path).exists()).cloned().collect();
    m.files.retain(|f| Path::new(&f.path).exists());
    if removed.is_empty() {
        return Ok(0);
    }
    for kind in [KIND_LOC, KIND_REL, KIND_REV] {
        ensure_log(&dir, kind)?;
        let mut f = OpenOptions::new()
            .append(true)
            .open(log_path(&dir, kind))
            .map_err(|e| e.to_string())?;
        for r in &removed {
            append_rec(&mut f, REC_FILE_REMOVE, &r.id.to_be_bytes()).map_err(|e| e.to_string())?;
        }
    }
    write_file_atomic(&manifest_path(&dir), &enc_manifest(&m)).map_err(|e| e.to_string())?;
    Ok(removed.len())
}

#[derive(Clone, Debug, Default)]
pub struct Status {
    pub version: u8,
    pub generation: u64,
    pub files: usize,
    pub uuid_total: u64,
    pub loc_blocks: usize,
    pub rel_blocks: usize,
    pub rev_blocks: usize,
    pub loc_entries: u64,
    pub rel_entries: u64,
    pub rev_entries: u64,
    pub log_bytes: u64,
    pub base_bytes: u64,
    /// 索引目录（含 manifest 与日志）的总字节数
    pub index_bytes: u64,
    pub stale_files: Vec<String>,
    pub need_compact: bool,
}

/// 文件表里的一行：谁被索引了、状态如何。
#[derive(Clone, Debug, Default)]
pub struct FileStatus {
    pub path: String,
    pub entries: u64,
    /// 块条目的基线代号；与 `cur_gen` 不等说明块已过期（走日志）
    pub gen: u64,
    pub cur_gen: u64,
    pub exists: bool,
    pub fresh: bool,
}

fn entry_sum(l: &LedgerInfo) -> u64 {
    l.blocks.iter().map(|b| b.count).sum()
}

fn dir_bytes(dir: &Path) -> u64 {
    let mut total = 0;
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            if let Ok(m) = e.metadata() {
                total += if m.is_dir() { dir_bytes(&e.path()) } else { m.len() };
            }
        }
    }
    total
}

/// 深信息：比 `status` 多出每本台账的条目数、索引目录总体积与协议版本。
pub fn info(ws_root: &Path) -> Result<Status, String> {
    let mut s = status(ws_root)?;
    let dir = index_dir(ws_root);
    let m = read_manifest(&dir)?;
    s.version = VERSION;
    s.loc_entries = entry_sum(&m.loc);
    s.rel_entries = entry_sum(&m.rel);
    s.rev_entries = entry_sum(&m.rev);
    s.uuid_total = m.files.iter().map(|f| f.uuid_count).sum();
    s.index_bytes = dir_bytes(&dir);
    Ok(s)
}

/// 文件清单（`xr index files`）。
pub fn files_status(ws_root: &Path) -> Result<Vec<FileStatus>, String> {
    let m = read_manifest(&index_dir(ws_root))?;
    let mut out: Vec<FileStatus> = m
        .files
        .iter()
        .map(|f| {
            let cur = match fingerprint_of(Path::new(&f.path)) {
                Some((s, sec, nsec)) => {
                    Path::new(&f.path).exists() && s == f.size && sec == f.mtime_sec && nsec == f.mtime_nsec
                }
                None => false,
            };
            FileStatus {
                path: f.path.clone(),
                entries: f.uuid_count,
                gen: f.gen,
                cur_gen: f.cur_gen,
                exists: Path::new(&f.path).exists(),
                fresh: cur,
            }
        })
        .collect();
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// 增量修复：只重扫指纹变了的文件（自愈），再顺带清理已消失的文件。
/// 返回（更新的文件数，追加的条目数，清理的文件数）。
pub fn update(ws_root: &Path) -> Result<(usize, u64, usize), String> {
    let stale: Vec<PathBuf> = files_status(ws_root)?
        .into_iter()
        .filter(|f| f.exists && !f.fresh)
        .map(|f| PathBuf::from(f.path))
        .collect();
    let mut entries = 0u64;
    for p in &stale {
        entries += append_file(ws_root, p)?.loc;
    }
    let removed = gc(ws_root)?;
    Ok((stale.len(), entries, removed))
}

/// 删除整个索引目录（只删索引，绝不碰数据文件）。返回（释放字节数，登记的文件数）。
pub fn drop_index(ws_root: &Path) -> Result<(u64, usize), String> {
    let dir = index_dir(ws_root);
    if !dir.exists() {
        return Ok((0, 0));
    }
    let bytes = dir_bytes(&dir);
    let files = read_manifest(&dir).map(|m| m.files.len()).unwrap_or(0);
    let _guard = lock_writer(&dir)?;
    fs::remove_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok((bytes, files))
}

/// 把指定文件从台账移除（写删除记录 + 从文件表删除）；数据文件不动。
pub fn forget_files(ws_root: &Path, paths: &[String]) -> Result<usize, String> {
    let dir = index_dir(ws_root);
    let _guard = lock_writer(&dir)?;
    let mut m = read_manifest(&dir)?;
    let targets: Vec<String> = paths
        .iter()
        .map(|p| {
            Path::new(p)
                .canonicalize()
                .map(|c| c.to_string_lossy().into_owned())
                .unwrap_or_else(|_| lexical_abs(p))
        })
        .collect();
    let doomed: Vec<FileEntry> =
        m.files.iter().filter(|f| targets.contains(&f.path)).cloned().collect();
    if doomed.is_empty() {
        return Ok(0);
    }
    for kind in [KIND_LOC, KIND_REL, KIND_REV] {
        ensure_log(&dir, kind)?;
        let mut f = OpenOptions::new()
            .append(true)
            .open(log_path(&dir, kind))
            .map_err(|e| e.to_string())?;
        for r in &doomed {
            append_rec(&mut f, REC_FILE_REMOVE, &r.id.to_be_bytes()).map_err(|e| e.to_string())?;
        }
    }
    let ids: HashSet<u32> = doomed.iter().map(|f| f.id).collect();
    m.files.retain(|f| !ids.contains(&f.id));
    write_file_atomic(&manifest_path(&dir), &enc_manifest(&m)).map_err(|e| e.to_string())?;
    Ok(doomed.len())
}

/// 清掉写者锁；返回（锁里记的 pid，那个进程是否还活着）。
pub fn unlock(ws_root: &Path) -> Result<Option<(i32, bool)>, String> {
    let path = lock_path(&index_dir(ws_root));
    if !path.exists() {
        return Ok(None);
    }
    let pid = fs::read_to_string(&path).ok().and_then(|s| s.trim().parse::<i32>().ok());
    let alive = pid.map(|p| unsafe { libc::kill(p, 0) } == 0).unwrap_or(false);
    fs::remove_file(&path).map_err(|e| e.to_string())?;
    Ok(pid.map(|p| (p, alive)))
}

/// 索引状态：块数、日志占比、指纹不符的文件、是否建议压实。
pub fn status(ws_root: &Path) -> Result<Status, String> {
    let dir = index_dir(ws_root);
    let m = read_manifest(&dir)?;
    let base_bytes = m.loc.base_bytes + m.rel.base_bytes + m.rev.base_bytes;
    let mut stale = Vec::new();
    for f in &m.files {
        match fingerprint_of(Path::new(&f.path)) {
            Some((s, sec, nsec)) if s == f.size && sec == f.mtime_sec && nsec == f.mtime_nsec => {}
            _ => stale.push(f.path.clone()),
        }
    }
    let log_bytes = m.loc.log_bytes + m.rel.log_bytes + m.rev.log_bytes;
    Ok(Status {
        version: VERSION,
        generation: m.generation,
        files: m.files.len(),
        uuid_total: m.files.iter().map(|f| f.uuid_count).sum(),
        loc_blocks: m.loc.blocks.len(),
        rel_blocks: m.rel.blocks.len(),
        rev_blocks: m.rev.blocks.len(),
        loc_entries: entry_sum(&m.loc),
        rel_entries: entry_sum(&m.rel),
        rev_entries: entry_sum(&m.rev),
        log_bytes,
        base_bytes,
        index_bytes: 0,
        stale_files: stale,
        // 小索引不提示（几 KB 的头部就能把比例顶上去，没有意义）
        need_compact: base_bytes >= 1_000_000
            && (log_bytes as f64) > (base_bytes as f64) * COMPACT_HINT_RATIO,
    })
}

// ---------------------------------------------------------------- 读：查询

/// 一个命中的位置：哪个文件、数据段起点、相对偏移、长度、所属树根。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    pub file: String,
    pub data_start: u64,
    pub off: u64,
    pub len: u64,
}

/// 读出命中位置的节点（只读那么几个字节，不整读文件）。
pub fn read_node_at_hit(h: &Hit) -> Result<crate::codec::Node, String> {
    let mut f = File::open(&h.file).map_err(|e| e.to_string())?;
    f.seek(std::io::SeekFrom::Start(h.data_start + h.off)).map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; h.len as usize];
    f.read_exact(&mut buf).map_err(|e| e.to_string())?;
    let mut off = 0usize;
    crate::codec::decode_node(&buf, &mut off).map_err(crate::tree::codec_error)
}

/// 一个打开的块文件：块内按 key 有序、条目定长，用「seek + 二分」定位，
/// 只读命中的那几条——这是台账「一次二分命中」的关键。
struct BlockFile {
    f: File,
    count: u64,
    size: usize,
    data_off: u64,
}

impl BlockFile {
    fn entry(&mut self, i: u64) -> Result<Vec<u8>, String> {
        self.f
            .seek(std::io::SeekFrom::Start(self.data_off + i * self.size as u64))
            .map_err(|e| e.to_string())?;
        let mut buf = vec![0u8; self.size];
        self.f.read_exact(&mut buf).map_err(|_| "F012：索引块缺失".to_string())?;
        Ok(buf)
    }

    /// 第一个 key >= target 的条目下标。
    fn lower_bound(&mut self, key: &[u8; 32], cmp_len: usize) -> Result<u64, String> {
        let (mut lo, mut hi) = (0u64, self.count);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let e = self.entry(mid)?;
            if e[..cmp_len] < key[..cmp_len] {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        Ok(lo)
    }
}

pub struct Reader {
    dir: PathBuf,
    manifest: Manifest,
    final_gen: HashMap<u32, u64>,
    loc_over: HashMap<(Uuid, u32), LocEntry>,
    rel_over: HashMap<(Uuid, Uuid), Vec<RelEntry>>,
    rev_over: HashMap<Uuid, Vec<RevEntry>>,
    removed: HashSet<u32>,
    data_starts: HashMap<u32, u64>,
    /// 路径 → 文件号（`covers` 要按路径查，别线性扫）
    path_ids: HashMap<String, u32>,
    /// 统计：打开了多少次索引文件（跨文件查询的开销代理指标）。
    pub opens: u64,
    pub blocks_read: u64,
    /// 指纹对不上、因而被忽略的文件。
    pub stale_files: Vec<String>,
}

impl Reader {
    pub fn open(ws_root: &Path) -> Result<Reader, String> {
        let dir = index_dir(ws_root);
        let manifest = read_manifest(&dir)?;
        // 打开时就核对块文件在不在：宁可大声报错，也不要静默返回「查不到」
        for kind in [KIND_LOC, KIND_REL, KIND_REV] {
            for b in &ledger_ref(&manifest, kind).blocks {
                if !dir.join(&b.name).is_file() {
                    return Err(format!("F012：索引块缺失（{}）", b.name));
                }
            }
        }
        let path_ids = manifest.files.iter().map(|f| (f.path.clone(), f.id)).collect();
        let mut r = Reader {
            dir,
            final_gen: manifest.files.iter().map(|f| (f.id, f.cur_gen)).collect(),
            manifest,
            loc_over: HashMap::new(),
            rel_over: HashMap::new(),
            rev_over: HashMap::new(),
            removed: HashSet::new(),
            data_starts: HashMap::new(),
            path_ids,
            opens: 0,
            blocks_read: 0,
            stale_files: Vec::new(),
        };
        r.load_logs()?;
        r.path_ids = r.manifest.files.iter().map(|f| (f.path.clone(), f.id)).collect();
        // 指纹核对：对不上的文件，其条目一律不可信
        let mut stale: Vec<u32> = Vec::new();
        for f in r.manifest.files.clone() {
            let ok = match fingerprint_of(Path::new(&f.path)) {
                Some((s, sec, nsec)) => s == f.size && sec == f.mtime_sec && nsec == f.mtime_nsec,
                None => false,
            };
            if !ok {
                stale.push(f.id);
                r.stale_files.push(f.path.clone());
            }
        }
        for id in stale {
            r.loc_over.retain(|(_, fid), _| *fid != id);
            r.rel_over.retain(|_, v| {
                v.retain(|e| e.file_id != id);
                !v.is_empty()
            });
            r.rev_over.retain(|_, v| {
                v.retain(|e| e.file_id != id);
                !v.is_empty()
            });
            r.removed.insert(id);
        }
        Ok(r)
    }

    fn load_logs(&mut self) -> Result<(), String> {
        for kind in [KIND_LOC, KIND_REL, KIND_REV] {
            let path = log_path(&self.dir, kind);
            if !path.exists() {
                continue;
            }
            let mut f = File::open(&path).map_err(|e| e.to_string())?;
            read_prefix(&mut f, kind, PART_LOG)?;
            let mut buf = Vec::new();
            f.read_to_end(&mut buf).map_err(|e| e.to_string())?;
            let mut off = 0usize;
            let mut cur: Option<(u32, u64)> = None;
            let mut pending: Vec<(u32, u64, Vec<u8>)> = Vec::new();
            while off + 5 <= buf.len() {
                let rlen = u32::from_be_bytes(buf[off..off + 4].try_into().unwrap()) as usize;
                let rk = buf[off + 4];
                off += 5;
                if off + rlen > buf.len() {
                    break; // 半条记录：保留前面解析出来的部分（日志是缓存，允许截断）
                }
                let payload = &buf[off..off + rlen];
                off += rlen;
                match rk {
                    REC_FILE => {
                        let mut o = 0usize;
                        let rec = rd_file_rec(payload, &mut o)?;
                        let e = self.final_gen.entry(rec.id).or_insert(rec.cur_gen);
                        if rec.cur_gen > *e {
                            *e = rec.cur_gen;
                        }
                        self.removed.remove(&rec.id);
                        if let Some(existing) = self.manifest.files.iter_mut().find(|x| x.id == rec.id)
                        {
                            *existing = rec.clone();
                        } else {
                            self.manifest.files.push(rec.clone());
                        }
                        cur = Some((rec.id, rec.cur_gen));
                    }
                    REC_FILE_REMOVE => {
                        let mut o = 0usize;
                        let id = rd_u32(payload, &mut o)?;
                        self.removed.insert(id);
                    }
                    REC_ENTRY => {
                        if let Some((fid, g)) = cur {
                            pending.push((fid, g, payload.to_vec()));
                        }
                    }
                    _ => {} // 未知记录：按长度跳过（向前兼容）
                }
            }
            for (fid, g, payload) in pending {
                if self.final_gen.get(&fid).copied().unwrap_or(g) != g {
                    continue; // 旧代号，作废
                }
                match kind {
                    KIND_LOC if payload.len() == LOC_ENTRY => {
                        let e = dec_loc(&payload);
                        self.loc_over.insert((e.uuid, e.file_id), e);
                    }
                    KIND_REL if payload.len() == REL_ENTRY => {
                        let e = dec_rel(&payload);
                        self.rel_over.entry((e.root, e.parent)).or_default().push(e);
                    }
                    KIND_REV if payload.len() == REV_ENTRY => {
                        let e = dec_rev(&payload);
                        self.rev_over.entry(e.target).or_default().push(e);
                    }
                    _ => {}
                }
            }
        }
        // 清掉已删除文件的日志条目
        let removed: Vec<u32> = self.removed.iter().copied().collect();
        if !removed.is_empty() {
            self.loc_over.retain(|(_, fid), _| !removed.contains(fid));
            self.rel_over.retain(|_, v| {
                v.retain(|e| !removed.contains(&e.file_id));
                !v.is_empty()
            });
            self.rev_over.retain(|_, v| {
                v.retain(|e| !removed.contains(&e.file_id));
                !v.is_empty()
            });
        }
        Ok(())
    }

    pub fn file_count(&self) -> usize {
        self.manifest.files.len()
    }

    /// 台账是否覆盖这个文件且指纹一致（工作区模式用来判断能不能直接用索引）。
    pub fn covers(&self, path: &str) -> bool {
        // 词法归一化（不 canonicalize）：几百个文件时这一句省掉几十毫秒
        let abs = lexical_abs(path);
        let id = self.path_ids.get(&abs).copied().or_else(|| {
            // 只有词法路径没命中时才退回 canonicalize（例如路径里有符号链接）
            Path::new(path)
                .canonicalize()
                .ok()
                .and_then(|c| self.path_ids.get(&c.to_string_lossy().into_owned()).copied())
        });
        match id.and_then(|id| self.manifest.files.iter().find(|f| f.id == id)) {
            Some(f) => match fingerprint_of(Path::new(&f.path)) {
                Some((s, sec, nsec)) => s == f.size && sec == f.mtime_sec && nsec == f.mtime_nsec,
                None => false,
            },
            None => false,
        }
    }

    fn path_of(&self, file_id: u32) -> Option<String> {
        self.manifest.files.iter().find(|f| f.id == file_id).map(|f| f.path.clone())
    }

    /// 块里的条目是否还对得上（文件没被追加过新代号）。
    fn block_valid(&self, file_id: u32) -> bool {
        if self.removed.contains(&file_id) {
            return false;
        }
        match self.manifest.files.iter().find(|f| f.id == file_id) {
            Some(f) => f.gen == *self.final_gen.get(&file_id).unwrap_or(&f.gen),
            None => false,
        }
    }

    fn file_fresh(&self, file_id: u32) -> bool {
        if self.removed.contains(&file_id) {
            return false;
        }
        match self.manifest.files.iter().find(|f| f.id == file_id) {
            Some(f) => match fingerprint_of(Path::new(&f.path)) {
                Some((s, sec, nsec)) => s == f.size && sec == f.mtime_sec && nsec == f.mtime_nsec,
                None => false,
            },
            None => false,
        }
    }

    fn data_start_of(&mut self, file_id: u32) -> Result<u64, String> {
        if let Some(v) = self.data_starts.get(&file_id) {
            return Ok(*v);
        }
        let path = self.path_of(file_id).ok_or("F012：索引块缺失")?;
        let mut f = File::open(&path).map_err(|e| e.to_string())?;
        let mut head = [0u8; 9];
        f.read_exact(&mut head).map_err(|_| "F003：文件头不完整".to_string())?;
        let hlen = u32::from_be_bytes(head[5..9].try_into().unwrap()) as u64;
        let start = 9 + hlen;
        self.data_starts.insert(file_id, start);
        Ok(start)
    }

    fn open_block(&mut self, kind: u8, info: &BlockInfo) -> Result<BlockFile, String> {
        self.opens += 1;
        self.blocks_read += 1;
        let mut f = File::open(self.dir.join(&info.name))
            .map_err(|_| "F012：索引块缺失".to_string())?;
        read_prefix(&mut f, kind, PART_BLOCK)?;
        let mut b8 = [0u8; 8];
        f.read_exact(&mut b8).map_err(|_| "F012：索引块缺失".to_string())?;
        Ok(BlockFile {
            f,
            count: u64::from_be_bytes(b8),
            size: entry_size(kind),
            data_off: fixed_prefix_len() as u64 + 8,
        })
    }

    /// 在候选块里按前缀匹配收集条目（块内二分 + 顺序扫描命中段）。
    fn collect_match(
        &mut self,
        kind: u8,
        key: &[u8; 32],
        cmp_len: usize,
    ) -> Result<Vec<Vec<u8>>, String> {
        let mut out = Vec::new();
        'outer: for b in self.candidate_blocks(kind, key, cmp_len) {
            let mut blk = self.open_block(kind, &b)?;
            let mut i = blk.lower_bound(key, cmp_len)?;
            while i < blk.count {
                let e = blk.entry(i)?;
                if e[..cmp_len] > key[..cmp_len] {
                    break 'outer;
                }
                if e[..cmp_len] == key[..cmp_len] {
                    out.push(e);
                }
                i += 1;
            }
        }
        Ok(out)
    }

    /// 找出可能包含 key 的块（从下界块开始，遇到首键已越过 key 的块就停）。
    fn candidate_blocks(&self, kind: u8, key: &[u8; 32], cmp_len: usize) -> Vec<BlockInfo> {
        let blocks = ledger_ref(&self.manifest, kind).blocks.clone();
        let mut lo = 0usize;
        let mut hi = blocks.len();
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if blocks[mid].last[..cmp_len] < key[..cmp_len] {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        let mut out = Vec::new();
        for b in blocks.iter().skip(lo) {
            if b.first[..cmp_len] > key[..cmp_len] {
                break;
            }
            out.push(b.clone());
        }
        out
    }

    fn hit(&mut self, file_id: u32, off: u64, len: u64) -> Result<Hit, String> {
        let file = self.path_of(file_id).ok_or("F012：索引块缺失")?;
        let data_start = self.data_start_of(file_id)?;
        Ok(Hit { file, data_start, off, len })
    }

    /// 该编号所属的树根（同编号多文件 → 可能有多个）。
    fn roots_of(&mut self, id: Uuid) -> Result<Vec<Uuid>, String> {
        let mut out: HashSet<Uuid> = HashSet::new();
        let key = key_uuid(id);
        for raw in self.collect_match(KIND_LOC, &key, 16)? {
            let e = dec_loc(&raw);
            if self.block_valid(e.file_id) && self.file_fresh(e.file_id) {
                out.insert(e.root);
            }
        }
        for ((u, fid), e) in self.loc_over.clone() {
            if u == id && self.file_fresh(fid) {
                out.insert(e.root);
            }
        }
        Ok(out.into_iter().collect())
    }

    /// 编号 → 位置（可能多份：同编号多文件）。
    ///
    /// **去重口径**：按「编号 + 文件」去重、**日志优先**。
    /// 追加写落地后，同一个编号在同一个文件里会既有块里的旧位置、又有日志里的新位置；
    /// 若按「文件 + 偏移」去重，同一个节点会被返回两次（`xr ws` 会显示「2 处」，
    /// 其实是一处），而且可能读到旧内容。日志里的那条才是当前状态，所以它覆盖块里的。
    pub fn locate(&mut self, id: Uuid) -> Result<Vec<Hit>, String> {
        // 键 = 文件路径；块先放，日志后放（后写覆盖 = 日志优先）
        let mut by_file: std::collections::BTreeMap<String, Hit> = std::collections::BTreeMap::new();
        let key = key_uuid(id);
        let hits: Vec<LocEntry> = self
            .collect_match(KIND_LOC, &key, 16)?
            .into_iter()
            .map(|raw| dec_loc(&raw))
            .filter(|e| self.block_valid(e.file_id) && self.file_fresh(e.file_id))
            .collect();
        for e in hits {
            let h = self.hit(e.file_id, e.off, e.len)?;
            by_file.insert(h.file.clone(), h);
        }
        for ((u, fid), e) in self.loc_over.clone() {
            if u == id && self.file_fresh(fid) {
                let h = self.hit(fid, e.off, e.len)?;
                by_file.insert(h.file.clone(), h); // 日志优先
            }
        }
        Ok(by_file.into_values().collect())
    }

    /// 孩子（并集）：先由父编号定位它的树根，再查关系本。
    pub fn children_of(&mut self, parent: Uuid) -> Result<Vec<Hit>, String> {
        let roots: Vec<Uuid> = self.roots_of(parent)?;
        let mut out: Vec<Hit> = Vec::new();
        let mut seen: HashSet<Uuid> = HashSet::new();
        for root in roots {
            for e in self.rel_entries(root, Some(parent))? {
                if seen.insert(e.child) {
                    out.push(self.hit(e.file_id, e.off, e.len)?);
                }
            }
        }
        Ok(out)
    }

    fn rel_entries(&mut self, root: Uuid, parent: Option<Uuid>) -> Result<Vec<RelEntry>, String> {
        let mut out: Vec<RelEntry> = Vec::new();
        if let Some(p) = parent {
            if let Some(v) = self.rel_over.get(&(root, p)) {
                out.extend(v.iter().cloned());
            }
        } else {
            for ((r, _), v) in &self.rel_over {
                if *r == root {
                    out.extend(v.iter().cloned());
                }
            }
        }
        let mut key = [0u8; 32];
        key[..16].copy_from_slice(&root.0);
        let cmp_len = if let Some(p) = parent {
            key[16..].copy_from_slice(&p.0);
            32
        } else {
            16
        };
        let hits: Vec<RelEntry> = self
            .collect_match(KIND_REL, &key, cmp_len)?
            .into_iter()
            .map(|raw| dec_rel(&raw))
            .filter(|e| self.block_valid(e.file_id) && self.file_fresh(e.file_id))
            .collect();
        for e in hits {
            out.push(e);
        }
        out.sort_by(|a, b| a.child.0.cmp(&b.child.0));
        out.dedup_by_key(|e| e.child);
        Ok(out)
    }

    /// 反向：谁引用我。
    pub fn references(&mut self, target: Uuid) -> Result<Vec<(String, Uuid)>, String> {
        let mut out: Vec<(String, Uuid)> = Vec::new();
        if let Some(v) = self.rev_over.get(&target).cloned() {
            for e in v {
                if let Some(p) = self.path_of(e.file_id) {
                    out.push((p, e.source));
                }
            }
        }
        let key = key_uuid(target);
        let hits: Vec<RevEntry> = self
            .collect_match(KIND_REV, &key, 16)?
            .into_iter()
            .map(|raw| dec_rev(&raw))
            .filter(|e| self.block_valid(e.file_id) && self.file_fresh(e.file_id))
            .collect();
        for e in hits {
            if let Some(p) = self.path_of(e.file_id) {
                out.push((p, e.source));
            }
        }
        out.sort_by(|a, b| a.1 .0.cmp(&b.1 .0));
        out.dedup();
        Ok(out)
    }

    /// 整棵子树：关系本按树根聚集，命中就是一段连续区间。
    pub fn subtree(&mut self, root: Uuid) -> Result<Vec<Hit>, String> {
        let mut out: Vec<Hit> = Vec::new();
        for e in self.rel_entries(root, None)? {
            out.push(self.hit(e.file_id, e.off, e.len)?);
        }
        Ok(out)
    }

    /// 树根自身的位置（用来从根开始读整棵树）。
    pub fn root_hit(&mut self, root: Uuid) -> Result<Option<Hit>, String> {
        Ok(self.locate(root)?.into_iter().next())
    }

    /// 抽样若干编号（`xr index check` 用）。
    /// 某个编号所属的顶层根（定位表里记着它）。
    pub fn root_of(&mut self, id: Uuid) -> Option<Uuid> {
        self.roots_of(id).ok().and_then(|v| v.into_iter().next())
    }

    /// **有界**地枚举引用边 `(源, 目标)`：顺序扫反向表，最多 `limit` 条。
    ///
    /// 只给「按根聚合的图 / 导出统计」这类需要整体视角的调用方用；
    /// 界面按视口取数应当走 `children_of` / `references` 这种按点查询。
    pub fn edges(&mut self, limit: usize) -> Vec<(Uuid, Uuid)> {
        let mut out: Vec<(Uuid, Uuid)> = Vec::new();
        for v in self.rev_over.values() {
            for e in v {
                out.push((e.source, e.target));
                if out.len() >= limit {
                    return out;
                }
            }
        }
        if let Some(info) = ledger_ref(&self.manifest, KIND_REV).blocks.first().cloned() {
            if let Ok(mut blk) = self.open_block(KIND_REV, &info) {
                let mut i = 0u64;
                while i < blk.count {
                    if let Ok(raw) = blk.entry(i) {
                        let e = dec_rev(&raw);
                        out.push((e.source, e.target));
                        if out.len() >= limit {
                            break;
                        }
                    }
                    i += 1;
                }
            }
        }
        out
    }

    /// 顶层根的**粗略枚举**：定位表里「自己是自己的根」的条目就是根。
    ///
    /// 台账没有单独的根清单，所以这是一次定位表扫描（按 `limit` 提前停止）；
    /// 只是给界面"库里有哪些条目"用的，拿到的编号仍可用 `locate` 复核。
    pub fn roots(&mut self, limit: usize) -> Vec<Uuid> {
        let mut out: Vec<Uuid> = Vec::new();
        let mut seen: std::collections::HashSet<Uuid> = std::collections::HashSet::new();
        for ((u, _), e) in &self.loc_over {
            if e.uuid == e.root && seen.insert(*u) {
                out.push(*u);
                if out.len() >= limit {
                    return out;
                }
            }
        }
        if let Some(info) = ledger_ref(&self.manifest, KIND_LOC).blocks.first().cloned() {
            if let Ok(mut blk) = self.open_block(KIND_LOC, &info) {
                let mut i = 0u64;
                while i < blk.count {
                    if let Ok(raw) = blk.entry(i) {
                        let e = dec_loc(&raw);
                        if e.uuid == e.root && seen.insert(e.uuid) {
                            out.push(e.uuid);
                            if out.len() >= limit {
                                break;
                            }
                        }
                    }
                    i += 1;
                }
            }
        }
        out
    }

    pub fn sample_nodes(&mut self, n: usize) -> Vec<Uuid> {
        let mut out: Vec<Uuid> = Vec::new();
        for ((u, _), _) in &self.loc_over {
            out.push(*u);
            if out.len() >= n {
                return out;
            }
        }
        if let Some(info) = ledger_ref(&self.manifest, KIND_LOC).blocks.first().cloned() {
            if let Ok(mut blk) = self.open_block(KIND_LOC, &info) {
                let mut i = 0u64;
                while i < blk.count {
                    if let Ok(raw) = blk.entry(i) {
                        out.push(dec_loc(&raw).uuid);
                    }
                    if out.len() >= n {
                        break;
                    }
                    i += 1;
                }
            }
        }
        out
    }
}
