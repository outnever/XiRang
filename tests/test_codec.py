"""codec 的测试：节点 ↔ 字节能无损往返（编码后再解码，等于原样）。"""
import uuid

from tools import codec
from tools.codec import Node


def test_value_roundtrip_int():
    for n in (0, 1, -1, 9223372036854775807, -9223372036854775808):
        v = (codec.INT, n)
        assert codec.decode_value(codec.encode_value(*v))[0] == v


def test_value_roundtrip_float():
    v = (codec.FLOAT, 3.141592653589793)
    assert codec.decode_value(codec.encode_value(*v))[0] == v


def test_value_roundtrip_bool():
    for b in (True, False):
        v = (codec.BOOL, b)
        assert codec.decode_value(codec.encode_value(*v))[0] == v


def test_value_roundtrip_text():
    for s in ("", "照明或发热的器具", "hello 世界"):
        v = (codec.TEXT, s)
        assert codec.decode_value(codec.encode_value(*v))[0] == v


def test_value_roundtrip_empty():
    v = (codec.EMPTY, None)
    assert codec.decode_value(codec.encode_value(*v))[0] == v


def test_value_roundtrip_reference():
    u = uuid.uuid4()
    v = (codec.REFERENCE, u)
    assert codec.decode_value(codec.encode_value(*v))[0] == v


def test_value_roundtrip_blob():
    for content in (b"", b"\x00\x01\x02\xff", bytes(range(256)) * 1000):
        v = (codec.BLOB, content)
        assert codec.decode_value(codec.encode_value(*v))[0] == v


def test_node_roundtrip():
    n = Node(
        id=uuid.uuid4(),
        parent=uuid.uuid4(),
        name="释义",
        value=(codec.TEXT, "照明或发热的器具"),
    )
    raw = codec.encode_node(n)
    n2, off = codec.decode_node(raw)
    assert n2 == n
    assert off == len(raw)  # 读完正好用完所有字节


def test_node_roundtrip_root_empty():
    # 根节点（parent=None）+ 空名字 + 空值（空类型）
    n = Node(id=uuid.uuid4(), parent=None, name="", value=(codec.EMPTY, None))
    n2, _ = codec.decode_node(codec.encode_node(n))
    assert n2 == n
    assert n2.parent is None


def test_name_over_255_bytes_raises():
    # "字" 是 3 字节 × 100 = 300 字节 > 255 上限
    n = Node(id=uuid.uuid4(), parent=None, name="字" * 100, value=(codec.EMPTY, None))
    try:
        codec.encode_node(n)
        assert False, "节点名超过 255 字节应该抛 ValueError"
    except ValueError:
        pass


def test_unknown_tag_raises():
    try:
        codec.encode_value(99, 0)
        assert False, "未知类型标记应该抛 ValueError"
    except ValueError:
        pass


def test_blob_not_bytes_raises():
    try:
        codec.encode_value(codec.BLOB, "不是字节")
        assert False, "二进制块的值不是 bytes 应该抛 TypeError"
    except (TypeError, ValueError):
        pass
