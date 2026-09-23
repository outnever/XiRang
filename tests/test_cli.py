"""CLI（tools/cli.py）冒烟测试：info / tree / validate 与错误处理。"""
import uuid

from tools import codec, tree
from tools.cli import main


def _build_store():
    store = tree.Store()
    root = codec.Node(id=uuid.uuid4(), parent=None, name="根", value=(codec.EMPTY, None))
    store.add(root)
    a = codec.Node(id=uuid.uuid4(), parent=root.id, name="甲", value=(codec.TEXT, "hello"))
    store.add(a)
    b = codec.Node(id=uuid.uuid4(), parent=root.id, name="乙", value=(codec.INT, 42))
    store.add(b)
    store.add(codec.Node(id=uuid.uuid4(), parent=root.id, name="指向甲",
                         value=(codec.REFERENCE, a.id)))
    store.add(codec.Node(id=uuid.uuid4(), parent=root.id, name="@note",
                         value=(codec.TEXT, "辅助")))
    return store


def test_info(tmp_path, capsys):
    store = _build_store()
    path = tmp_path / "t.xirang"
    store.save(path)
    assert main(["info", str(path)]) == 0
    out = capsys.readouterr().out
    assert "格式版本: 1" in out
    assert "节点数: 5" in out
    assert "根节点数: 1" in out


def test_tree(tmp_path, capsys):
    store = _build_store()
    path = tmp_path / "t.xirang"
    store.save(path)
    assert main(["tree", str(path)]) == 0
    out = capsys.readouterr().out
    assert "根" in out
    assert "甲 = hello" in out
    assert "乙 = 42" in out
    assert "指向甲 = → 甲" in out
    assert "@note = 辅助" in out


def test_tree_skip_aux(tmp_path, capsys):
    store = _build_store()
    path = tmp_path / "t.xirang"
    store.save(path)
    assert main(["tree", str(path), "--skip-aux"]) == 0
    out = capsys.readouterr().out
    assert "甲 = hello" in out
    assert "@note" not in out


def test_validate_ok(tmp_path, capsys):
    store = _build_store()
    path = tmp_path / "t.xirang"
    store.save(path)
    assert main(["validate", str(path)]) == 0
    out = capsys.readouterr().out
    assert "0 错误" in out


def test_bad_magic(tmp_path, capsys):
    path = tmp_path / "bad.xirang"
    path.write_bytes(b"XXXX-not-xirang")
    assert main(["info", str(path)]) == 2
    err = capsys.readouterr().err
    assert "F001" in err
