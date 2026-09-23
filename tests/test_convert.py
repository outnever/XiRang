"""convert 的测试：Store ↔ JSON / XML / YAML 无损往返；Markdown 有损导出。"""
import hashlib
import json
import uuid

import pytest

from tools import codec
from tools.codec import Node
from tools.tree import Store
from tools import convert


def _node(store, name, value, parent):
    n = Node(id=uuid.uuid4(), parent=parent, name=name, value=value)
    store.add(n)
    return n


def _build():
    store = Store()
    guangyuan = _node(store, "光源", (codec.TEXT, "能发光的物体"), None)
    deng = _node(store, "灯", (codec.EMPTY, None), None)
    _node(store, "词形", (codec.TEXT, "灯"), deng.id)
    _node(store, "词频", (codec.INT, 2046), deng.id)
    _node(store, "释义", (codec.TEXT, "照明或做其他用途的发光器具"), deng.id)
    _node(store, "父类", (codec.REFERENCE, guangyuan.id), deng.id)
    _node(store, "亮度", (codec.FLOAT, 3.14), deng.id)
    _node(store, "亮着", (codec.BOOL, True), deng.id)
    _node(store, "头像", (codec.BLOB, b"\x00\x01\x02PNG\xff"), deng.id)
    return store


def _assert_same(a, b):
    assert len(a) == len(b)
    for n in a.nodes():
        m = b.get(n.id)
        assert m is not None, f"缺节点 {n.name}"
        assert m.parent == n.parent
        assert m.name == n.name
        assert m.value == n.value


def test_json_flat_roundtrip():
    store = _build()
    store2 = convert.from_json(convert.to_json(store, add_fingerprint=False))
    _assert_same(store, store2)


def test_json_nested_roundtrip():
    store = _build()
    store2 = convert.from_json(convert.to_json(store, nested=True, add_fingerprint=False))
    _assert_same(store, store2)


def test_xml_roundtrip():
    store = _build()
    store2 = convert.from_xml(convert.to_xml(store, add_fingerprint=False))
    _assert_same(store, store2)


def test_yaml_roundtrip():
    store = _build()
    store2 = convert.from_yaml(convert.to_yaml(store, add_fingerprint=False))
    _assert_same(store, store2)


def test_blob_external_roundtrip(tmp_path):
    store = _build()
    text = convert.to_json(store, blob_dir=str(tmp_path), add_fingerprint=False)
    store2 = convert.from_json(text, blob_dir=str(tmp_path))
    _assert_same(store, store2)


def test_add_fingerprint():
    store = _build()
    blob = [n for n in store.nodes() if n.value[0] == codec.BLOB][0]
    expected = hashlib.sha256(blob.value[1]).hexdigest()
    text = convert.to_json(store, add_fingerprint=True)  # 默认不补，需显式开启
    assert "@fingerprint" in text
    assert expected in text


def test_add_fingerprint_idempotent():
    # blob 已有 @fingerprint 时不再重复补
    store = Store()
    blob = _node(store, "头像", (codec.BLOB, b"abc"), None)
    _node(store, "@fingerprint", (codec.TEXT, "deadbeef"), blob.id)
    text = convert.to_json(store, add_fingerprint=True)
    assert text.count("@fingerprint") == 1


def test_json_default_has_no_fingerprint():
    # 默认导出与 Rust / 规范一致：不额外补 @fingerprint
    store = _build()
    assert "@fingerprint" not in convert.to_json(store)


def test_md_has_content():
    store = _build()
    md = convert.to_md(store)
    assert "光源" in md
    assert "2046" in md
    assert "→ 光源" in md
    assert "[二进制块]" in md


# —— C 错误码 ——

def _json_nodes(nodes):
    return json.dumps({"format": "xirang", "kernel": "1.0", "nodes": nodes})


def test_C001_unknown_type():
    text = _json_nodes([{"id": str(uuid.uuid4()), "parent": None, "name": "x",
                         "value": {"type": "string", "value": "y"}}])
    with pytest.raises(ValueError, match="C001"):
        convert.from_json(text)


def test_C002_bad_uuid():
    text = _json_nodes([{"id": "abc", "parent": None, "name": "x",
                         "value": {"type": "text", "value": ""}}])
    with pytest.raises(ValueError, match="C002"):
        convert.from_json(text)


def test_C003_bad_base64():
    text = _json_nodes([{"id": str(uuid.uuid4()), "parent": None, "name": "x",
                         "value": {"type": "blob", "value": "***", "encoding": "base64"}}])
    with pytest.raises(ValueError, match="C003"):
        convert.from_json(text)


def test_C004_missing_ref_file(tmp_path):
    text = _json_nodes([{"id": str(uuid.uuid4()), "parent": None, "name": "x",
                         "value": {"type": "blob", "ref": "blob_none.bin"}}])
    with pytest.raises(ValueError, match="C004"):
        convert.from_json(text, blob_dir=str(tmp_path))


def test_C005_missing_field():
    text = _json_nodes([{"id": str(uuid.uuid4()), "parent": None, "name": "x"}])
    with pytest.raises(ValueError, match="C005"):
        convert.from_json(text)


# —— 减量导出（slim）——

def _build_slim():
    store = Store()
    guangyuan = _node(store, "光源", (codec.TEXT, "能发光的物体"), None)
    deng = _node(store, "灯", (codec.EMPTY, None), None)
    _node(store, "词形", (codec.TEXT, "灯"), deng.id)
    _node(store, "词频", (codec.INT, 2046), deng.id)
    _node(store, "父类", (codec.REFERENCE, guangyuan.id), deng.id)
    _node(store, "亮度", (codec.FLOAT, 3.14), deng.id)
    _node(store, "亮着", (codec.BOOL, True), deng.id)
    _node(store, "头像", (codec.BLOB, b"abc"), deng.id)
    _node(store, "@note", (codec.TEXT, "说明"), deng.id)
    liangci = _node(store, "量词", (codec.EMPTY, None), deng.id)
    _node(store, "", (codec.TEXT, "个"), liangci.id)
    _node(store, "", (codec.TEXT, "只"), liangci.id)
    fuhe = _node(store, "复合", (codec.TEXT, "有值"), None)
    _node(store, "子A", (codec.TEXT, "A"), fuhe.id)
    return store


def _find(store, name):
    for n in store.nodes():
        if n.name == name:
            return n
    return None


def test_slim_json_scalars():
    store = _build_slim()
    text = convert.to_json_slim(store)
    assert "@note" not in text
    assert "[二进制块 3 字节]" in text
    store2 = convert.from_json_slim(text)
    # 标量值对（按名字查）
    assert _find(store2, "词频").value == (codec.INT, 2046)
    assert _find(store2, "亮度").value == (codec.FLOAT, 3.14)
    assert _find(store2, "亮着").value == (codec.BOOL, True)
    assert _find(store2, "词形").value == (codec.TEXT, "灯")
    # 引用 → 文本（目标名）
    assert _find(store2, "父类").value == (codec.TEXT, "光源")
    # @ 被排除
    assert _find(store2, "@note") is None
    # 编号是新生成的
    assert _find(store2, "词频").id != _find(store, "词频").id


def test_slim_map_and_list():
    store = _build_slim()
    store2 = convert.from_json_slim(convert.to_json_slim(store))
    deng = _find(store2, "灯")
    assert {c.name for c in store2.children(deng)} >= {"词形", "词频", "量词"}
    liangci = _find(store2, "量词")
    assert [n.value[1] for n in store2.children(liangci)] == ["个", "只"]


def test_slim_value_plus_children():
    store = _build_slim()
    store2 = convert.from_json_slim(convert.to_json_slim(store))
    fuhe = _find(store2, "复合")
    assert fuhe.value == (codec.TEXT, "有值")
    assert [c.name for c in store2.children(fuhe)] == ["子A"]


def test_slim_yaml_roundtrip():
    store = _build_slim()
    store2 = convert.from_yaml_slim(convert.to_yaml_slim(store))
    assert _find(store2, "词频").value == (codec.INT, 2046)
    assert _find(store2, "词形").value == (codec.TEXT, "灯")
    assert _find(store2, "父类").value == (codec.TEXT, "光源")


def test_slim_duplicate_name():
    # 同名兄弟节点：map 形态，后者覆盖前者
    store = Store()
    root = _node(store, "根", (codec.EMPTY, None), None)
    _node(store, "同名", (codec.TEXT, "第一个"), root.id)
    _node(store, "同名", (codec.TEXT, "第二个"), root.id)
    store2 = convert.from_json_slim(convert.to_json_slim(store))
    root2 = _find(store2, "根")
    tongming = [c for c in store2.children(root2) if c.name == "同名"]
    assert len(tongming) == 1
    assert tongming[0].value == (codec.TEXT, "第二个")
