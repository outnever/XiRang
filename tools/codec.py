"""息壤节点编解码器（v1.0）。

把「节点」（四属性）变成字节、再变回来。格式见 README。

字节序约定：所有多字节整数用**大端序**（网络字节序）。

一个节点的字节布局（无头部、无版本号）：

    [节点编号 16 字节] [父节点 16 字节] [节点名] [节点值]

- 节点名 = [1 字节长度] [UTF-8 字节]
- 节点值 = [类型标记 1 字节] [内容]
- 父节点 = nil UUID（16 字节全 0）表示「根节点」

值的大类（类型标记）与内容：

    0 空        无内容（纯容器节点，只有子节点）
    1 整数      8 字节（有符号 int64，大端）
    2 浮点数    8 字节（IEEE 754 double，大端）
    3 布尔      1 字节（0 = 假，1 = 真）
    4 文本      [4 字节长度] [UTF-8 字节]
    5 引用      16 字节 UUID（构成关联边）
    6 二进制块  [8 字节长度] [原始字节]

兼容性：旧标记（0-6）永不复用；要改就新增标记。
"""

import struct
import uuid
from dataclasses import dataclass
from typing import Optional, Tuple

# —— 大类标记（类型标记，1 字节）——
EMPTY = 0      # 空：纯容器节点，无值内容
INT = 1        # 整数
FLOAT = 2      # 浮点数
BOOL = 3       # 布尔
TEXT = 4       # 文本
REFERENCE = 5  # 引用：UUID，构成关联边
BLOB = 6       # 二进制块：内联原始字节

TAG_NAMES = {
    EMPTY: "空",
    INT: "整数",
    FLOAT: "浮点数",
    BOOL: "布尔",
    TEXT: "文本",
    REFERENCE: "引用",
    BLOB: "二进制块",
}

NIL_UUID = uuid.UUID(int=0)  # 全 0，代表「空」（根节点）


@dataclass
class Node:
    """息壤节点（四属性）。

    字段：
      id:     节点编号（UUID）
      parent: 父节点编号（UUID）；None 表示根节点
      name:   节点名（str）
      value:  节点值，形如 (tag, data)：
                (EMPTY, None)
                (INT, int)
                (FLOAT, float)
                (BOOL, bool)
                (TEXT, str)
                (REFERENCE, uuid.UUID)
                (BLOB, bytes)
    """
    id: uuid.UUID
    parent: Optional[uuid.UUID]
    name: str
    value: Tuple[int, object]


# ---------------------------------------------------------------------------
# 编码（节点 → 字节）
# ---------------------------------------------------------------------------

def _enc_short_text(s: str) -> bytes:
    """短文本（节点名）：1 字节长度 + UTF-8，上限 255 字节。"""
    raw = s.encode("utf-8")
    if len(raw) > 255:
        raise ValueError(f"文本超过 255 字节上限（{len(raw)} 字节）：{s!r}")
    return bytes([len(raw)]) + raw


def _enc_text(s: str) -> bytes:
    """文本值：4 字节长度 + UTF-8。"""
    raw = s.encode("utf-8")
    return len(raw).to_bytes(4, "big") + raw


def encode_value(tag: int, data: object) -> bytes:
    """把节点值编码成字节（含类型标记）。"""
    if tag == EMPTY:
        return bytes([EMPTY])
    if tag == INT:
        return bytes([INT]) + data.to_bytes(8, "big", signed=True)
    if tag == FLOAT:
        return bytes([FLOAT]) + struct.pack(">d", data)
    if tag == BOOL:
        return bytes([BOOL]) + (b"\x01" if data else b"\x00")
    if tag == TEXT:
        return bytes([TEXT]) + _enc_text(data)
    if tag == REFERENCE:
        return bytes([REFERENCE]) + data.bytes
    if tag == BLOB:
        return bytes([BLOB]) + len(data).to_bytes(8, "big") + data
    raise ValueError(f"未知类型标记：{tag}")


def encode_node(node: Node) -> bytes:
    """把节点编码成字节。"""
    pid = node.parent if node.parent is not None else NIL_UUID
    return (
        node.id.bytes
        + pid.bytes
        + _enc_short_text(node.name)
        + encode_value(node.value[0], node.value[1])
    )


# ---------------------------------------------------------------------------
# 解码（字节 → 节点）
# ---------------------------------------------------------------------------

def _dec_short_text(data: bytes, off: int) -> Tuple[str, int]:
    n = data[off]
    off += 1
    return data[off:off + n].decode("utf-8"), off + n


def _dec_text(data: bytes, off: int) -> Tuple[str, int]:
    n = int.from_bytes(data[off:off + 4], "big")
    off += 4
    return data[off:off + n].decode("utf-8"), off + n


def decode_value(data: bytes, off: int = 0) -> Tuple[Tuple[int, object], int]:
    """从字节解析节点值，返回 ((tag, data), 新偏移)。"""
    tag = data[off]
    off += 1
    if tag == EMPTY:
        v = None
    elif tag == INT:
        v = int.from_bytes(data[off:off + 8], "big", signed=True)
        off += 8
    elif tag == FLOAT:
        v = struct.unpack(">d", data[off:off + 8])[0]
        off += 8
    elif tag == BOOL:
        v = data[off] != 0
        off += 1
    elif tag == TEXT:
        v, off = _dec_text(data, off)
    elif tag == REFERENCE:
        v = uuid.UUID(bytes=data[off:off + 16])
        off += 16
    elif tag == BLOB:
        size = int.from_bytes(data[off:off + 8], "big")
        off += 8
        v = data[off:off + size]
        off += size
    else:
        raise ValueError(f"未知类型标记：{tag}")
    return (tag, v), off


def decode_node(data: bytes, off: int = 0) -> Tuple[Node, int]:
    """从字节解析节点，返回 (Node, 新偏移)。"""
    nid = uuid.UUID(bytes=data[off:off + 16])
    off += 16
    pid = uuid.UUID(bytes=data[off:off + 16])
    off += 16
    parent = None if pid == NIL_UUID else pid
    name, off = _dec_short_text(data, off)
    value, off = decode_value(data, off)
    return Node(id=nid, parent=parent, name=name, value=value), off
