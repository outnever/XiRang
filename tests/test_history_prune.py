"""Python 参考实现：留痕裁剪（与 Rust 的 `Store::prune_history` 语义对齐）。

对齐点：
- 保留最近 N 条快照；@history 节点本身保留；
- `before` 是与 @replaced 做字符串比较的 ISO 前缀；
- 被裁的快照是真正的结构删除（节点数减少）；
- 跨语言交叉验证：Python 裁过的文件，Rust 侧读回来校验通过。
"""
import subprocess
import uuid
from pathlib import Path

import pytest

from tools import codec, tree


def _store_with_edits(n=20):
    """造一个被改过 n 次的库：根节点 + @history 快照。"""
    store = tree.Store()
    root = codec.Node(id=uuid.uuid4(), parent=None, name="根", value=(codec.EMPTY, None))
    store.add(root)
    for i in range(n):
        store.update(root, (codec.TEXT, f"值{i}"))
    return store, root


def _snapshots(store, root):
    hist = next(c for c in store.children(root) if c.name == "@history")
    return store.children(hist)


def test_prune_keeps_newest_and_shrinks():
    store, root = _store_with_edits(20)
    assert len(_snapshots(store, root)) == 20
    before_nodes = len(store)

    removed, kept = store.prune_history(root, keep=5)
    assert removed == 15
    assert kept == 5
    snaps = _snapshots(store, root)
    assert len(snaps) == 5
    # 快照存的是「改之前的值」：20 次修改留下 [空, 值0 … 值18]，所以最近 5 条是 值14..值18
    assert [s.value[1] for s in snaps] == [f"值{i}" for i in range(14, 19)]
    assert len(store) == before_nodes - 15 * 2, "每条快照连同它的 @replaced 一起被删"
    assert any(c.name == "@history" for c in store.children(root))


def test_prune_all_and_no_history_are_noops():
    store, root = _store_with_edits(3)
    assert store.prune_history(root, keep=0) == (3, 0)
    assert _snapshots(store, root) == []
    assert store.prune_history(root, keep=0) == (0, 0)

    plain = tree.Store()
    r = codec.Node(id=uuid.uuid4(), parent=None, name="无留痕", value=(codec.EMPTY, None))
    plain.add(r)
    assert plain.prune_history(r) == (0, 0), "没有 @history 时不该报错"


def test_prune_before_only_removes_older_snapshots():
    store, root = _store_with_edits(4)
    hist = next(c for c in store.children(root) if c.name == "@history")
    snaps = store.children(hist)
    for s, when in zip(
        snaps,
        [
            "2000-01-01T00:00:00+00:00",
            "2000-01-02T00:00:00+00:00",
            "2025-12-31T00:00:00+00:00",
        ],
    ):
        for c in store.children(s):
            if c.name == "@replaced":
                c.value = (codec.TEXT, when)
    removed, kept = store.prune_history(root, keep=0, before="2026-01-01")
    assert removed == 3, "早于 2026-01-01 的三条应被裁"
    assert kept == 1
    assert len(_snapshots(store, root)) == 1


def test_python_pruned_file_is_valid_for_rust(tmp_path):
    """跨语言：Python 裁过的文件，Rust 侧应能读、能校验、能数出保留的快照。"""
    release = Path("rust/target/release/xr")
    debug = Path("rust/target/debug/xr")
    binary = release if release.exists() else debug
    if not binary.exists():
        pytest.skip("没有 Rust 二进制（先 cargo build）")

    store, root = _store_with_edits(10)
    store.prune_history(root, keep=2)
    path = tmp_path / "pruned.xirang"
    store.save(path)

    out = subprocess.run([str(binary), "validate", str(path)], capture_output=True, text=True)
    assert out.returncode == 0, out.stdout + out.stderr
    assert "0 错误" in out.stdout

    hist = subprocess.run(
        [str(binary), "history", str(path), str(root.id)], capture_output=True, text=True
    )
    assert hist.returncode == 0
    assert hist.stdout.count("快照：") == 2, hist.stdout
