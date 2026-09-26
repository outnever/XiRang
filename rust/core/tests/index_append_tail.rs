//! 只追加登记（`wsidx::append_tail`）的护栏。
//!
//! 台账的有效性规则只有一条：条目属于「写它时那个文件记录」的代号。
//! - 整份登记 → 代号 +1，旧条目作废；
//! - 只追加登记 → **代号一个字都不动**，旧条目继续有效，只补变化的那几条。
//!
//! 这里逐条钉死：结果必须与「整份载入」一致、前缀被改写必须拒绝、
//! 引用改掉要写墓碑、父节点在本次新增里也要能定树、重复调用是空操作。

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};

use xirang_core::codec::{self, Uuid, Value};
use xirang_core::index::Backend as _;
use xirang_core::{fixture, index, shard, tree, wsidx};

fn tmp_dir(tag: &str) -> PathBuf {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let d = std::env::temp_dir().join(format!("xr-tail-{tag}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn sort(it: impl Iterator<Item = Uuid>) -> Vec<Uuid> {
    let set: HashSet<[u8; 16]> = it.map(|u| u.0).collect();
    let mut v: Vec<[u8; 16]> = set.into_iter().collect();
    v.sort();
    v.into_iter().map(Uuid).collect()
}

/// 每项 = (点查, 孩子, 反向来源, 整树)，与 `index_parity.rs` 同口径。
fn snapshot(
    b: &mut dyn index::Backend,
    ids: &[Uuid],
) -> Vec<(Vec<Uuid>, Vec<Uuid>, Vec<Uuid>, Vec<Uuid>)> {
    ids.iter()
        .map(|id| {
            let loc = sort(b.locate(*id).into_iter().map(|(_, n)| n.id));
            let kids = sort(b.children(*id).into_iter().map(|(_, n)| n.id));
            let rev = sort(b.references(*id).into_iter().map(|(_, s)| s));
            let sub = sort(b.subtree(*id).into_iter().map(|(_, n)| n.id));
            (loc, kids, rev, sub)
        })
        .collect()
}

fn file_entry(ws_root: &Path, path: &Path) -> wsidx::FileStatus {
    let want = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    wsidx::files_status(ws_root)
        .unwrap()
        .into_iter()
        .find(|f| Path::new(&f.path) == want)
        .expect("台账里应该有这个文件")
}

fn append_bytes(path: &Path, bytes: &[u8]) {
    let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    f.write_all(bytes).unwrap();
}

/// 只追加登记之后：台账结果与「整份载入」逐条一致，而且块没被作废。
#[test]
fn append_tail_agrees_with_full_load_and_keeps_blocks() {
    let dir = tmp_dir("agree");
    let spec = fixture::FixtureSpec {
        kind: fixture::FixtureKind::Multi,
        files: 2,
        entries_per_file: 30,
        inherit_ratio: 1,
        seed: 21,
        name: "tail".into(),
    };
    let info = fixture::build(&dir, &spec).unwrap();
    wsidx::rebuild(&dir, &[]).unwrap();
    let paths: Vec<String> =
        info.files.iter().map(|p| p.to_string_lossy().into_owned()).collect();

    // 改一个词形的值，然后**只追加**（旧记录一个字节都不动）
    let word = info.word_ids[0];
    let before = tree::Store::load_view(&info.files[0]).unwrap();
    let mut after = before.clone();
    after.update(word, Value::Text("改过的词形".into())).unwrap();
    shard::append_changes(&info.files[0], &before, &after).unwrap();

    let rep = wsidx::append_tail(&dir, &info.files[0]).unwrap();
    assert_eq!(rep.truncated_bytes, 0, "正常追加不该有残片");
    assert!(rep.stats.bytes_written > 0, "应该有日志写入");
    assert!(rep.stats.loc > 0 && rep.stats.rel > 0, "新记录要进定位本与关系本");

    // 代号没动 → 块条目继续有效（这就是「只追加登记」的全部意义）
    let f = file_entry(&dir, &info.files[0]);
    assert_eq!(f.gen, f.cur_gen, "只追加登记不该推进代号");
    assert!(f.fresh, "登记完就是新鲜的");

    // 台账 == 整份载入
    let mut ids: Vec<Uuid> = vec![word];
    ids.extend(info.entry_ids.iter().take(10).copied());
    ids.extend(info.word_ids.iter().take(10).copied());
    ids.extend(info.referenced_ids.iter().take(10).copied());
    let mut ws = index::WorkspaceBackend::open(&dir).unwrap();
    let mut mem = index::MemoryBackend::load(&paths).unwrap();
    assert_eq!(snapshot(&mut ws, &ids), snapshot(&mut mem, &ids), "只追加登记后与整份载入不一致");

    // 再来一次：没有新字节 → 空操作（不写台账、不报错）
    let rep2 = wsidx::append_tail(&dir, &info.files[0]).unwrap();
    assert_eq!(rep2.stats.bytes_written, 0, "没有新增时不该写台账");

    // 压实之后仍然一致
    wsidx::compact(&dir).unwrap();
    let mut ws = index::WorkspaceBackend::open(&dir).unwrap();
    let mut mem = index::MemoryBackend::load(&paths).unwrap();
    assert_eq!(snapshot(&mut ws, &ids), snapshot(&mut mem, &ids), "压实后不一致");
    std::fs::remove_dir_all(&dir).ok();
    std::fs::remove_dir_all(wsidx::index_dir(&dir)).ok();
}

/// 父节点也在本次新增里（`import --append` 加一整棵子树），而且故意「子在前、父在后」。
#[test]
fn append_tail_resolves_parents_inside_the_new_tail() {
    let dir = tmp_dir("subtree");
    let spec = fixture::FixtureSpec {
        kind: fixture::FixtureKind::Single,
        files: 1,
        entries_per_file: 5,
        inherit_ratio: 1,
        seed: 3,
        name: "sub".into(),
    };
    let info = fixture::build(&dir, &spec).unwrap();
    wsidx::rebuild(&dir, &[]).unwrap();
    let path = info.files[0].clone();

    let mut s = tree::Store::load_view(&path).unwrap();
    let root = s.create(None, "新根", Value::Empty, false);
    let child = s.create(Some(root.id), "新子", Value::Text("值".into()), false);
    let grand = s.create(Some(child.id), "新孙", Value::Empty, false);
    let mut bytes = Vec::new();
    for n in [child.id, root.id, grand.id] {
        bytes.extend(codec::encode_node(s.get(n).unwrap()).unwrap());
    }
    append_bytes(&path, &bytes);

    let rep = wsidx::append_tail(&dir, &path).unwrap();
    assert_eq!(rep.stats.loc, 3, "三条新记录都要登记");
    assert_eq!(rep.stats.rel, 2, "两条父子边");
    assert_eq!(rep.stats.rev, 0, "没有引用值");

    let paths = vec![path.to_string_lossy().into_owned()];
    let mut ws = index::WorkspaceBackend::open(&dir).unwrap();
    let mut mem = index::MemoryBackend::load(&paths).unwrap();
    let ids = vec![root.id, child.id, grand.id];
    assert_eq!(snapshot(&mut ws, &ids), snapshot(&mut mem, &ids));
    assert_eq!(
        sort(ws.children(root.id).into_iter().map(|(_, n)| n.id)),
        vec![child.id],
        "新子树的关系本要对"
    );
    std::fs::remove_dir_all(&dir).ok();
    std::fs::remove_dir_all(wsidx::index_dir(&dir)).ok();
}

/// 引用被改掉：旧的那条「旧目标 → 这个节点」必须写墓碑作废。
#[test]
fn append_tail_writes_tombstone_when_reference_changes() {
    let dir = tmp_dir("rev");
    let path = dir.join("引用.xirang");
    let mut s = tree::Store::new();
    let root = s.create(None, "根", Value::Empty, false);
    let holder = s.create(Some(root.id), "甲", Value::Empty, false);
    let y = s.create(Some(root.id), "乙", Value::Empty, false);
    let z = s.create(Some(root.id), "丙", Value::Empty, false);
    let edge = s.create(Some(holder.id), "指向", Value::Reference(y.id), false);
    s.save(&path).unwrap();
    wsidx::rebuild(&dir, &[]).unwrap();

    let ids = vec![holder.id, y.id, z.id, edge.id];
    let paths = vec![path.to_string_lossy().into_owned()];
    let mut ws = index::WorkspaceBackend::open(&dir).unwrap();
    assert_eq!(
        sort(ws.references(y.id).into_iter().map(|(_, s)| s)),
        vec![edge.id],
        "改之前：乙 被「指向」引着"
    );

    // 从引用「乙」改成引用「丙」，只追加
    let store = tree::Store::load_view(&path).unwrap();
    let mut next = store.clone();
    next.update(edge.id, Value::Reference(z.id)).unwrap();
    shard::append_changes(&path, &store, &next).unwrap();
    wsidx::append_tail(&dir, &path).unwrap();

    let mut ws = index::WorkspaceBackend::open(&dir).unwrap();
    let mut mem = index::MemoryBackend::load(&paths).unwrap();
    assert_eq!(snapshot(&mut ws, &ids), snapshot(&mut mem, &ids), "改完引用后与整份载入不一致");
    // 旧边必须作废。注意「乙」不是完全没人引用——`@history` 里那条 `@replaced`
    // 记的就是旧值（引用「乙」），那是**数据**、必须留着；作废的只是「指向」这条旧边。
    let from_y = sort(ws.references(y.id).into_iter().map(|(_, s)| s));
    assert!(!from_y.contains(&edge.id), "旧引用边必须作废：「指向」不该再引着乙（实得 {from_y:?}）");
    assert_eq!(
        sort(ws.references(z.id).into_iter().map(|(_, s)| s)),
        vec![edge.id],
        "新引用边要生效"
    );
    assert_eq!(ws.locate(edge.id).len(), 1, "改引用的节点本身还在");
    std::fs::remove_dir_all(&dir).ok();
    std::fs::remove_dir_all(wsidx::index_dir(&dir)).ok();
}

/// 前缀被改写（长度一点不变也抓住）→ 必须拒绝，让调用方退回整份登记。
#[test]
fn append_tail_refuses_when_prefix_was_rewritten() {
    let dir = tmp_dir("guard");
    let spec = fixture::FixtureSpec {
        kind: fixture::FixtureKind::Single,
        files: 1,
        entries_per_file: 5,
        inherit_ratio: 1,
        seed: 9,
        name: "guard".into(),
    };
    let info = fixture::build(&dir, &spec).unwrap();
    wsidx::rebuild(&dir, &[]).unwrap();
    let path = info.files[0].clone();
    let mut s = tree::Store::load_view(&path).unwrap();
    let extra = s.create(None, "新根", Value::Empty, false);
    append_bytes(&path, &codec::encode_node(s.get(extra.id).unwrap()).unwrap());
    wsidx::append_tail(&dir, &path).unwrap();

    // 在「已登记区间的最后一个字节」上翻一位：长度完全不变
    let len_now = std::fs::metadata(&path).unwrap().len();
    let mut raw = std::fs::read(&path).unwrap();
    raw[len_now as usize - 1] ^= 0xff;
    std::fs::write(&path, &raw).unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().len(), len_now, "长度没变：只有守护哈希能抓住");

    // 再追加一条：总长变了，但前缀对不上 → 必须拒绝
    append_bytes(&path, &codec::encode_node(s.get(extra.id).unwrap()).unwrap());
    assert!(wsidx::append_tail(&dir, &path).is_err(), "前缀被改写过就必须拒绝");

    std::fs::remove_dir_all(&dir).ok();
    std::fs::remove_dir_all(wsidx::index_dir(&dir)).ok();
}

/// 台账里没有这个文件 → 明确要求整份登记，而不是就地猜。
#[test]
fn append_tail_refuses_unregistered_file() {
    let dir = tmp_dir("unreg");
    let path = dir.join("没登记过.xirang");
    let mut s = tree::Store::new();
    s.create(None, "根", Value::Empty, false);
    s.save(&path).unwrap();
    assert!(wsidx::append_tail(&dir, &path).is_err());
    std::fs::remove_dir_all(&dir).ok();
    std::fs::remove_dir_all(wsidx::index_dir(&dir)).ok();
}
