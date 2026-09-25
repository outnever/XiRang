"""Python 参考实现的留痕裁剪（与 Rust 语义对齐），含一条跨语言一致性校验。

对齐点：保留最近 N 条快照、@history 节点本身保留、before 与 @replaced 做字符串比较、
被裁的快照连同 @replaced 真正删除。
"""
import subprocess
import uuid
from pathlib import Path

import pytest

from tools import codec, tree


def _store_with_edits(n=20):
    store = tree.Store()
    root = codec.Node(id=uuid.uuid4(), parent=None, name="根", value=(codec.EMPTY, None))
    store.add(root)
    for i in range(n):
        store.update(root, (codec.TEXT, f"值{i}"))
    return store, root


def _snapshots(store, root):
    hist = next(c for c in store.children(root) if c.name == "@history")
    return store.children(hist)


def test_prune_keeps_newest_and_drops_the_rest():
    store, root = _store_with_edits(20)
    before_nodes = len(store)
    removed, kept = store.prune_history(root, keep=5)
    assert (removed, kept) == (15, 5)
    snaps = _snapshots(store, root)
    # 快照存的是「改之前的值」：20 次修改留下 [空, 值0 … 值18]
    assert [s.value[1] for s in snaps] == [f"值{i}" for i in range(14, 19)]
    assert len(store) == before_nodes - 15 * 2, "每条快照连同它的 @replaced 一起被删"
    assert any(c.name == "@history" for c in store.children(root)), "@history 本身保留"


def test_prune_is_a_noop_without_history():
    store = tree.Store()
    root = codec.Node(id=uuid.uuid4(), parent=None, name="无留痕", value=(codec.EMPTY, None))
    store.add(root)
    assert store.prune_history(root) == (0, 0)


def test_python_pruned_file_is_valid_for_rust(tmp_path):
    """跨语言一致性：Python 裁过的文件，Rust 侧能读、能校验、能数出保留的快照。"""
    binary = Path("rust/target/release/xr")
    if not binary.exists():
        binary = Path("rust/target/debug/xr")
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
