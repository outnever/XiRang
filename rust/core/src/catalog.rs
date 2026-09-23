//! 本机全局目录（catalog）：用户级、跨项目的 `UUID → 文件路径` 轻量索引，用于跨库连接。
//! 只写用户级目录文件，绝不改动 `.xirang`；当缓存用，可整体删除重建。
//!
//! 布局（大端）：文件段（路径 + 指纹 + 条数）在前，后面是**按 UUID 排序**的条目段。
//! 查找时只读文件段 + 在条目段上「磁盘二分」，不整体加载目录。

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use crate::codec::Uuid;
use crate::tree::Store;

pub const MAGIC: &[u8; 5] = b"XRCAT";
pub const VERSION: u8 = 2;
pub const ENTRY_SIZE: u64 = 21; // uuid16 + file_index u32 + flags u8
pub const HEADER: &str = "\
XiRang local catalog (XRCAT) v2
User-level index: maps node UUID -> file path, for cross-library lookup. Cache; rebuildable.
Layout (big-endian):
  magic \"XRCAT\" (5) + version (1) + header_len u32 + this header
  + file_count u64
  + file_count x ( path_len u32, path bytes, size u64, mtime_sec i64, mtime_nsec u32, uuid_count u64 )
  + entry_count u64
  + entry_count x ( uuid16, file_index u32, flags u8 )   sorted by (uuid, file_index); flags reserved (always 0)
";

const VERSION_MISMATCH: &str = "XRCAT_VERSION_MISMATCH";

/// 文件指纹：尺寸 + mtime（秒 / 纳秒），用于失效判定。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fingerprint {
    pub size: u64,
    pub mtime_sec: i64,
    pub mtime_nsec: u32,
}

pub fn fingerprint(path: &Path) -> Option<Fingerprint> {
    let meta = fs::metadata(path).ok()?;
    let (mtime_sec, mtime_nsec) = match meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
    {
        Some(d) => (d.as_secs() as i64, d.subsec_nanos()),
        None => (0, 0),
    };
    Some(Fingerprint { size: meta.len(), mtime_sec, mtime_nsec })
}

/// 默认目录文件：`$XIRANG_CATALOG`，否则 `~/.config/xirang/catalog.idx`。
pub fn default_path() -> PathBuf {
    if let Some(p) = std::env::var_os("XIRANG_CATALOG") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("xirang").join("catalog.idx")
}

/// 一个已索引的文件（内存态）：路径 + 指纹 + 它包含的 UUID 集。
#[derive(Clone, Debug)]
pub struct FileRecord {
    pub path: String,
    pub fp: Fingerprint,
    pub uuids: Vec<Uuid>,
}

impl FileRecord {
    pub fn contains(&self, id: Uuid) -> bool {
        self.uuids.iter().any(|u| *u == id)
    }
}

/// 文件段里的一条（路径 + 指纹 + UUID 条数）。
#[derive(Clone, Debug)]
pub struct FileHead {
    pub path: String,
    pub fp: Fingerprint,
    pub uuid_count: u64,
}

fn corrupt() -> String {
    "W006：目录损坏".to_string()
}

fn read_u8(f: &mut fs::File) -> Result<u8, String> {
    let mut b = [0u8; 1];
    f.read_exact(&mut b).map_err(|_| corrupt())?;
    Ok(b[0])
}
fn read_u32(f: &mut fs::File) -> Result<u32, String> {
    let mut b = [0u8; 4];
    f.read_exact(&mut b).map_err(|_| corrupt())?;
    Ok(u32::from_be_bytes(b))
}
fn read_u64(f: &mut fs::File) -> Result<u64, String> {
    let mut b = [0u8; 8];
    f.read_exact(&mut b).map_err(|_| corrupt())?;
    Ok(u64::from_be_bytes(b))
}
fn read_i64(f: &mut fs::File) -> Result<i64, String> {
    let mut b = [0u8; 8];
    f.read_exact(&mut b).map_err(|_| corrupt())?;
    Ok(i64::from_be_bytes(b))
}

/// 从文件头读到条目段起点：返回 (文件段, 条目段偏移, 条目数)。只读文件段，不碰条目。
fn read_heads_from(f: &mut fs::File) -> Result<(Vec<FileHead>, u64, u64), String> {
    f.seek(SeekFrom::Start(0)).map_err(|_| corrupt())?;
    let mut magic = [0u8; 5];
    f.read_exact(&mut magic).map_err(|_| corrupt())?;
    if &magic != MAGIC {
        return Err(corrupt());
    }
    if read_u8(f)? != VERSION {
        return Err(VERSION_MISMATCH.to_string());
    }
    let hlen = read_u32(f)? as usize;
    let mut hbuf = vec![0u8; hlen];
    f.read_exact(&mut hbuf).map_err(|_| corrupt())?;

    let file_count = read_u64(f)? as usize;
    let mut heads = Vec::with_capacity(file_count);
    for _ in 0..file_count {
        let plen = read_u32(f)? as usize;
        let mut pbuf = vec![0u8; plen];
        f.read_exact(&mut pbuf).map_err(|_| corrupt())?;
        let path = String::from_utf8(pbuf).map_err(|_| corrupt())?;
        let size = read_u64(f)?;
        let mtime_sec = read_i64(f)?;
        let mtime_nsec = read_u32(f)?;
        let uuid_count = read_u64(f)?;
        heads.push(FileHead { path, fp: Fingerprint { size, mtime_sec, mtime_nsec }, uuid_count });
    }
    let entry_count = read_u64(f)?;
    let entries_off = f.stream_position().map_err(|_| corrupt())?;
    Ok((heads, entries_off, entry_count))
}

/// 读取第 i 条排序条目：(uuid, file_index, flags)。
fn read_entry(f: &mut fs::File, entries_off: u64, i: u64) -> Result<(Uuid, u32, u8), String> {
    f.seek(SeekFrom::Start(entries_off + i * ENTRY_SIZE)).map_err(|_| corrupt())?;
    let mut b = [0u8; 16];
    f.read_exact(&mut b).map_err(|_| corrupt())?;
    let fidx = read_u32(f)?;
    let flags = read_u8(f)?;
    Ok((Uuid(b), fidx, flags))
}

/// 只读目录（磁盘二分查找用）：文件段驻内存，条目段按需 seek。
pub struct CatalogReader {
    file: fs::File,
    files: Vec<FileHead>,
    entries_off: u64,
    entry_count: u64,
    cache: HashMap<Uuid, Vec<String>>,
}

impl CatalogReader {
    pub fn open(path: &Path) -> Result<CatalogReader, String> {
        let mut file = fs::File::open(path).map_err(|_| corrupt())?;
        let (files, entries_off, entry_count) = read_heads_from(&mut file)?;
        Ok(CatalogReader { file, files, entries_off, entry_count, cache: HashMap::new() })
    }

    /// 二分 + 顺序扫同一 UUID 的连续段 → **所有**包含该编号的文件路径。
    /// 同一个编号出现在多个文件里是正常现象，所以是「多份」不是「一份」。
    pub fn lookup_all(&mut self, id: Uuid) -> Result<Vec<String>, String> {
        if let Some(hit) = self.cache.get(&id) {
            return Ok(hit.clone());
        }
        let (mut lo, mut hi) = (0u64, self.entry_count);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let (u, _, _) = read_entry(&mut self.file, self.entries_off, mid)?;
            if u.0 < id.0 {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        let mut result: Vec<String> = Vec::new();
        let mut i = lo;
        while i < self.entry_count {
            let (u, fidx, _flags) = read_entry(&mut self.file, self.entries_off, i)?;
            if u.0 != id.0 {
                break;
            }
            if let Some(h) = self.files.get(fidx as usize) {
                if !result.contains(&h.path) {
                    result.push(h.path.clone());
                }
            }
            i += 1;
        }
        self.cache.insert(id, result.clone());
        Ok(result)
    }
}

#[derive(Default)]
pub struct Catalog {
    pub files: Vec<FileRecord>,
}

impl Catalog {
    /// 读取目录（整体加载，供检查 / 更新 / 列表）。
    /// 文件不存在或版本不匹配 → 空目录；结构损坏 → W006。
    pub fn load(path: &Path) -> Result<Catalog, String> {
        let mut f = match fs::File::open(path) {
            Ok(f) => f,
            Err(_) => return Ok(Catalog::default()),
        };
        let (heads, entries_off, entry_count) = match read_heads_from(&mut f) {
            Ok(x) => x,
            Err(e) if e == VERSION_MISMATCH => return Ok(Catalog::default()),
            Err(e) => return Err(e),
        };
        let mut files: Vec<FileRecord> = heads
            .iter()
            .map(|h| FileRecord {
                path: h.path.clone(),
                fp: h.fp,
                uuids: Vec::new(),
            })
            .collect();
        f.seek(SeekFrom::Start(entries_off)).map_err(|_| corrupt())?;
        for _ in 0..entry_count {
            let mut b = [0u8; 16];
            f.read_exact(&mut b).map_err(|_| corrupt())?;
            let uuid = Uuid(b);
            let fidx = read_u32(&mut f)?;
            let _flags = read_u8(&mut f)?; // 预留位（owner/shadow 语义已废弃，恒为 0）
            if let Some(rec) = files.get_mut(fidx as usize) {
                rec.uuids.push(uuid);
            }
        }
        Ok(Catalog { files })
    }

    /// 原子落盘：文件段 + 按 (UUID, 文件下标) 排序的条目段。失败静默（缓存可重建）。
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let mut entries: Vec<(Uuid, u32, u8)> = Vec::new();
        for (i, f) in self.files.iter().enumerate() {
            for u in &f.uuids {
                entries.push((*u, i as u32, 0u8)); // 预留位恒为 0
            }
        }
        entries.sort_by(|a, b| a.0 .0.cmp(&b.0 .0).then_with(|| a.1.cmp(&b.1)));

        let header = HEADER.as_bytes();
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        out.push(VERSION);
        out.extend_from_slice(&(header.len() as u32).to_be_bytes());
        out.extend_from_slice(header);
        out.extend_from_slice(&(self.files.len() as u64).to_be_bytes());
        for f in &self.files {
            out.extend_from_slice(&(f.path.as_bytes().len() as u32).to_be_bytes());
            out.extend_from_slice(f.path.as_bytes());
            out.extend_from_slice(&f.fp.size.to_be_bytes());
            out.extend_from_slice(&f.fp.mtime_sec.to_be_bytes());
            out.extend_from_slice(&f.fp.mtime_nsec.to_be_bytes());
            out.extend_from_slice(&(f.uuids.len() as u64).to_be_bytes());
        }
        out.extend_from_slice(&(entries.len() as u64).to_be_bytes());
        for (u, fidx, flags) in &entries {
            out.extend_from_slice(&u.0);
            out.extend_from_slice(&fidx.to_be_bytes());
            out.push(*flags);
        }

        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let tmp = tmp_path(path);
        fs::write(&tmp, &out)?;
        fs::rename(&tmp, path)?;
        Ok(())
    }

    /// 只读文件段判断「某文件是否已按同指纹索引」——最便宜的路径。
    pub fn is_fresh(path: &Path, file_path: &str, fp: Fingerprint) -> bool {
        let mut f = match fs::File::open(path) {
            Ok(f) => f,
            Err(_) => return false,
        };
        match read_heads_from(&mut f) {
            Ok((heads, _, _)) => heads.iter().any(|h| h.path == file_path && h.fp == fp),
            Err(_) => false,
        }
    }

    pub fn upsert(&mut self, file_path: &str, fp: Fingerprint, uuids: &[Uuid]) {
        if let Some(rec) = self.files.iter_mut().find(|f| f.path == file_path) {
            rec.fp = fp;
            rec.uuids = uuids.to_vec();
        } else {
            self.files.push(FileRecord {
                path: file_path.to_string(),
                fp,
                uuids: uuids.to_vec(),
            });
        }
        self.files.sort_by(|a, b| a.path.cmp(&b.path));
    }

    pub fn remove_file(&mut self, file_path: &str) {
        self.files.retain(|f| f.path != file_path);
    }

    pub fn files(&self) -> &[FileRecord] {
        &self.files
    }

    /// 内存态查找（整体已加载时用）：返回**所有**包含该编号的文件路径。
    /// 同一个编号出现在多个文件里是正常现象。
    pub fn lookup(&self, id: Uuid) -> Vec<&str> {
        self.files
            .iter()
            .filter(|f| f.contains(id))
            .map(|f| f.path.as_str())
            .collect()
    }

    /// 同一编号出现在 >=2 个文件里的那些编号（信息性列表：**不是**冲突）。
    pub fn duplicated(&self) -> Vec<(Uuid, Vec<String>)> {
        let mut by_uuid: HashMap<Uuid, Vec<String>> = HashMap::new();
        for f in &self.files {
            for u in &f.uuids {
                by_uuid.entry(*u).or_default().push(f.path.clone());
            }
        }
        let mut out: Vec<(Uuid, Vec<String>)> =
            by_uuid.into_iter().filter(|(_, v)| v.len() >= 2).collect();
        out.sort_by(|a, b| a.0 .0.cmp(&b.0 .0));
        out
    }

    /// 从已加载的 Store 取其全部 UUID。
    pub fn store_uuids(store: &Store) -> Vec<Uuid> {
        store.nodes().iter().map(|n| n.id).collect()
    }
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(format!(".tmp-{}", std::process::id()));
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(n: u32) -> Uuid {
        let mut b = [0u8; 16];
        b[12..16].copy_from_slice(&n.to_be_bytes());
        Uuid(b)
    }

    fn fp(size: u64) -> Fingerprint {
        Fingerprint { size, mtime_sec: 1, mtime_nsec: 2 }
    }

    fn tmp(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("xirang_cat_{tag}_{}", Uuid::random_v4()))
    }

    #[test]
    fn roundtrip_and_binary_search_lookup() {
        let p = tmp("rt");
        let mut c = Catalog::default();
        c.upsert("/a.xirang", fp(10), &[u(1), u(3), u(5)]);
        c.upsert("/b.xirang", fp(20), &[u(2), u(4)]);
        c.save(&p).unwrap();

        let mut r = CatalogReader::open(&p).unwrap();
        assert_eq!(r.lookup_all(u(1)).unwrap(), vec!["/a.xirang".to_string()]);
        assert_eq!(r.lookup_all(u(4)).unwrap(), vec!["/b.xirang".to_string()]);
        assert!(r.lookup_all(u(99)).unwrap().is_empty());

        let back = Catalog::load(&p).unwrap();
        assert_eq!(back.files().len(), 2);
        assert_eq!(back.lookup(u(3)), vec!["/a.xirang"]);
        assert!(Catalog::is_fresh(&p, "/a.xirang", fp(10)));
        assert!(!Catalog::is_fresh(&p, "/a.xirang", fp(11)));
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn same_uuid_in_many_files_is_normal() {
        let p = tmp("dup");
        let mut c = Catalog::default();
        c.upsert("/a.xirang", fp(1), &[u(1), u(7)]);
        c.upsert("/b.xirang", fp(1), &[u(1), u(8)]);
        // 同编号多文件 = 正常：两边都能查到，不再有 owner / 摒除
        let mut got = c.lookup(u(1));
        got.sort();
        assert_eq!(got, vec!["/a.xirang", "/b.xirang"]);
        assert_eq!(c.lookup(u(8)), vec!["/b.xirang"]);
        assert_eq!(c.duplicated().len(), 1);
        c.save(&p).unwrap();

        let mut r = CatalogReader::open(&p).unwrap();
        assert_eq!(
            r.lookup_all(u(1)).unwrap(),
            vec!["/a.xirang".to_string(), "/b.xirang".to_string()]
        );
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn version_mismatch_is_empty() {
        let p = tmp("ver");
        let mut c = Catalog::default();
        c.upsert("/a.xirang", fp(1), &[u(1)]);
        c.save(&p).unwrap();
        let mut data = fs::read(&p).unwrap();
        data[5] = 99;
        fs::write(&p, &data).unwrap();
        assert!(Catalog::load(&p).unwrap().files().is_empty());
        assert!(CatalogReader::open(&p).is_err());
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn upsert_refresh_and_remove() {
        let mut c = Catalog::default();
        c.upsert("/a.xirang", fp(1), &[u(1), u(2)]);
        c.upsert("/a.xirang", fp(2), &[u(1)]);
        assert_eq!(c.files().len(), 1);
        assert_eq!(c.files()[0].uuids.len(), 1);
        c.remove_file("/a.xirang");
        assert!(c.files().is_empty());
    }
}
