"""树的处理层（实现层）：把「一批节点」当树 / 图来读写、遍历、寻址，并提供实现层解读。

息壤内核只有「节点」；树 / 图 / 数组 / map / 枚举 / 时间都是**实现层的解读**，本文件提供这些便利方法。

基础操作：
- Store：一批节点 + 索引（UUID → 节点）
- children / roots / walk：遍历
- resolve：解析引用（引用 → 目标节点）
- encode / decode：树字节（无头，内存 / 传输用）
- save / load：文件字节（带魔数 + 版本 + 头，落盘用；头 = 一段纯文本自描述）

实现层解读（不是新格式，是「换个角度读同一批节点」）：
- children()：把子节点当**数组**读（追加序 = 有序）
- child_by_name() / as_map()：把子节点当 **map** 读（节点名 = key）
- resolve()：把引用当**枚举 / 关联**读（引用 → 目标节点）
- as_time()：把整数（时间戳）/ 文本（ISO）当**时间**读
"""

import uuid
from datetime import datetime, timezone

from tools import codec
from tools.codec import Node


# 文件头：一段纯文本自描述，放在文件最前，让不懂息壤的人 / 大模型也能读懂。
# 全英文（对模型更通用），给出明确的「逐步解析方法」。
# 注意：文件头只做「描述」，不内嵌任何可执行代码（安全原因见 README 的「⚠️ 安全提醒」节）。
MAGIC = b"XRNG"       # 文件魔数：4 个 ASCII 字节
FORMAT_VERSION = 1    # 文件格式版本（1 字节，单调递增）

HEADER = (
    "XiRang Tree v1.0\n"
    "\n"
    "This file stores a XiRang node tree in binary. All multi-byte integers are big-endian\n"
    "(most significant byte first; e.g. the bytes 00 00 00 2A represent the integer 42).\n"
    "\n"
    "FILE LAYOUT:\n"
    "  - The file begins with a fixed 5-byte prefix: magic \"XRNG\" (4 ASCII bytes) + a 1-byte\n"
    "    format version (currently 1).\n"
    "  - After the prefix: 4 bytes = the byte-length L of this text header (big-endian),\n"
    "    then L bytes of UTF-8 text (this documentation, NOT node data).\n"
    "  - The nodes begin at offset (5 + 4 + L).\n"
    "\n"
    "HOW TO PARSE A NODE:\n"
    "Each node = 4 fields, read in this exact order:\n"
    "  1. node_id:   read 16 bytes = a UUID (unique id of this node)\n"
    "  2. parent_id: read 16 bytes = a UUID (id of this node's parent; all 0x00 = a root node)\n"
    "  3. name:      read 1 byte = length N (in bytes), then read N bytes = UTF-8 text (the name)\n"
    "  4. value:     read 1 byte = type tag T, then read the content as listed below\n"
    "\n"
    "VALUE CONTENT BY TYPE TAG T:\n"
    "  T=0 empty:     read nothing = a container node (no value, children only)\n"
    "  T=1 integer:   read 8 bytes = signed 64-bit integer (big-endian)\n"
    "  T=2 float:     read 8 bytes = IEEE 754 double (big-endian)\n"
    "  T=3 boolean:   read 1 byte = 0x00 is false, 0x01 is true\n"
    "  T=4 text:      read 4 bytes = length M (big-endian), then read M bytes = UTF-8 text\n"
    "  T=5 reference: read 16 bytes = a UUID that points to another node's node_id (a link/edge)\n"
    "  T=6 blob:      read 8 bytes = length N (big-endian), then read N bytes = raw content\n"
    "\n"
    "EXAMPLES (hex; big-endian):\n"
    "  ENCODE (value -> bytes):\n"
    "    empty (T=0)                    -> 00\n"
    "    integer 2046 (T=1)             -> 01 00 00 00 00 00 00 07 fe\n"
    "    float 1.5 (T=2)                -> 02 3f f8 00 00 00 00 00 00\n"
    "    boolean true (T=3)             -> 03 01\n"
    "    text \"灯\" (T=4)                -> 04 00 00 00 03 e7 81 af\n"
    "    reference to a node_id (T=5)   -> 05 <16 bytes of the target node_id>\n"
    "    blob of 3 raw bytes (T=6)      -> 06 00 00 00 00 00 00 00 03 <3 bytes>\n"
    "  DECODE (bytes -> value), reverse of the above:\n"
    "    00                              -> empty\n"
    "    01 00 00 00 00 00 00 07 fe      -> integer 2046\n"
    "    04 00 00 00 03 e7 81 af         -> text \"灯\"\n"
    "    05 <16 bytes>                   -> reference to that node_id\n"
    "  A name field \"灯\" (UTF-8 e7 81 af) -> 03 e7 81 af   (1-byte length, then UTF-8)\n"
    "\n"
    "After reading a node's value, the node is complete; the next node (if any) starts\n"
    "immediately at the next byte. Read nodes until end of file.\n"
)


def _make_file(nodes_bytes: bytes) -> bytes:
    """把「纯节点字节」包成「文件字节」：[魔数][版本][头长][头文本][节点字节]。"""
    header = HEADER.encode("utf-8")
    return MAGIC + bytes([FORMAT_VERSION]) + len(header).to_bytes(4, "big") + header + nodes_bytes


def _parse_file(data: bytes) -> bytes:
    """从「文件字节」里剥掉魔数 + 版本 + 头，返回「纯节点字节」；非法则抛 F 码。"""
    if data[:4] != MAGIC:
        raise ValueError("F001：魔数非法（不是 XRNG 息壤文件）")
    version = data[4]
    if version != FORMAT_VERSION:
        raise ValueError(f"F002：格式版本不支持（{version}）")
    n = int.from_bytes(data[5:9], "big")
    if n < 0 or 9 + n > len(data):
        raise ValueError("F003：头长非法")
    return data[9 + n:]


class Store:
    """一批节点 + 索引。树 / 图都只是 Store 里的一堆节点。"""

    def __init__(self, nodes=None):
        self._nodes = {}   # uuid -> Node（索引：按编号寻址）
        self._order = []   # 追加序（数组 / 顺序靠它）
        for n in (nodes or []):
            self.add(n)

    # —— 基础操作 ——
    def add(self, node):
        self._nodes[node.id] = node
        self._order.append(node.id)

    def get(self, node_id):
        """按编号取节点（寻址）；不存在返回 None。"""
        return self._nodes.get(node_id)

    def __contains__(self, node_id):
        return node_id in self._nodes

    def __len__(self):
        return len(self._nodes)

    def nodes(self):
        """所有节点，按追加序。"""
        return [self._nodes[i] for i in self._order]

    def roots(self):
        """所有根节点（父节点为 nil）。"""
        return [n for n in self.nodes() if n.parent is None]

    def children(self, node, skip_aux=False):
        """子节点，按追加序——数组解读。skip_aux=True 时跳过 `@` 辅助节点。"""
        out = [n for n in self.nodes() if n.parent == node.id]
        if skip_aux:
            out = [n for n in out if not n.name.startswith("@")]
        return out

    def aux(self, node):
        """节点的辅助子节点（节点名以 `@` 开头）。"""
        return [n for n in self.children(node) if n.name.startswith("@")]

    def child_by_name(self, node, name):
        """按节点名取子节点——map 解读；找不到返回 None。"""
        for n in self.children(node):
            if n.name == name:
                return n
        return None

    def as_map(self, node):
        """把子节点读成 dict（节点名 = key）——map 解读。"""
        return {n.name: n for n in self.children(node)}

    def resolve(self, node):
        """解析引用：值若是引用，返回目标节点；否则返回 None——枚举 / 关联解读。"""
        tag, data = node.value
        if tag == codec.REFERENCE:
            return self.get(data)
        return None

    # —— 查的补全 ——
    def parent(self, node):
        """父节点；根节点返回 None。"""
        return self.get(node.parent) if node.parent is not None else None

    def references_to(self, node):
        """引用指向该节点的所有节点（谁引用了我）。"""
        return [n for n in self.nodes()
                if n.value[0] == codec.REFERENCE and n.value[1] == node.id]

    def subtree(self, node):
        """以 node 为根的子树节点列表（含 node，先根序）。"""
        return walk(self, node)

    def find(self, name=None, value=None):
        """按节点名 / 值查找，返回匹配节点列表。"""
        out = []
        for n in self.nodes():
            if name is not None and n.name != name:
                continue
            if value is not None and n.value != value:
                continue
            out.append(n)
        return out

    # —— 写操作（值可原位改 + 留痕）——
    def new(self, parent, name, value, record_created=True):
        """便捷增：自动生成编号。record_created=True 时挂 @created 时间辅助节点。"""
        n = codec.Node(id=uuid.uuid4(),
                       parent=parent.id if parent is not None else None,
                       name=name, value=value)
        self.add(n)
        if record_created:
            self._add_time(n, "@created")
        return n

    def update(self, node, new_value):
        """改：旧值复制到本节点的 @history（快照 + @replaced），原位改值。编号不变。"""
        history = self._ensure_history(node)
        snap = codec.Node(id=uuid.uuid4(), parent=history.id,
                          name=node.name, value=node.value)
        self.add(snap)
        self._add_time(snap, "@replaced")
        node.value = new_value
        return node

    def remove(self, node):
        """删：除「编号」外的字段（名字 + 值）置空，旧值进本节点 @history（快照 + @replaced）。

        与现行规范 / Rust 实现一致：节点留在原位、编号保留（空槽位），引用仍指向它。
        已是空节点则不动，避免「删了个空节点却留痕」。
        """
        if not node.name and node.value == (codec.EMPTY, None):
            return node
        history = self._ensure_history(node)
        snap = codec.Node(id=uuid.uuid4(), parent=history.id,
                          name=node.name, value=node.value)
        self.add(snap)
        self._add_time(snap, "@replaced")
        node.name = ""
        node.value = (codec.EMPTY, None)
        return node

    def _ensure_history(self, parent):
        """确保 parent 节点下有 @history 辅助节点，返回它（parent 为 None = 顶层）。"""
        if parent is None:
            for r in self.roots():
                if r.name == "@history":
                    return r
        else:
            for c in self.children(parent):
                if c.name == "@history":
                    return c
        return self.new(parent, "@history", (codec.EMPTY, None), record_created=False)

    def _add_time(self, node, name):
        """给 node 挂一个时间辅助节点（@created / @replaced）。"""
        t = codec.Node(id=uuid.uuid4(), parent=node.id, name=name,
                       value=(codec.TEXT, datetime.now(timezone.utc).isoformat()))
        self.add(t)
        return t

    # —— 落盘 ——
    def encode(self):
        """整库编码成一串字节（节点逐个接起来）。"""
        return b"".join(codec.encode_node(n) for n in self.nodes())

    @classmethod
    def decode(cls, data):
        """从字节重建 Store。"""
        store = cls()
        off = 0
        try:
            while off < len(data):
                node, off = codec.decode_node(data, off)
                store.add(node)
        except (IndexError, ValueError):
            raise ValueError("F004：文件截断（节点流在节点中间结束）")
        return store

    def save(self, path):
        """落盘：写「文件 = 魔数 + 版本 + 头 + 节点字节」。"""
        with open(path, "wb") as f:
            f.write(_make_file(self.encode()))

    @classmethod
    def load(cls, path):
        """读回：剥掉魔数 + 版本 + 头，再解析节点。"""
        with open(path, "rb") as f:
            return cls.decode(_parse_file(f.read()))


def walk(store, root, skip_aux=False):
    """从 root 深度优先遍历（先根序），沿父边；返回节点列表。skip_aux=True 时跳过 `@` 辅助节点。"""
    out = []

    def visit(n):
        out.append(n)
        for c in store.children(n, skip_aux=skip_aux):
            visit(c)

    visit(root)
    return out


def as_time(node):
    """时间解读：整数 = Unix 秒时间戳 → ISO 文本；文本 = 原样返回（本身就是时间）。"""
    tag, data = node.value
    if tag == codec.INT:
        return datetime.fromtimestamp(data, tz=timezone.utc).isoformat()
    if tag == codec.TEXT:
        return data
    return None
