"""树的处理层测试：用 CiBase 的真实词条结构做实例。

实例取自 CiBase：
- 「灯」：名词，取自 design/属性/名词专属属性.md 的 JSON 示例（含 数组/引用/map/闭枚）。
- 「包袱」：名词，取自 design/属性/通用属性.md 的 JSON 示例（多词义 + 词素引用）。
"""
import uuid

from tools import codec
from tools.codec import Node
from tools.tree import Store, walk, as_time


def _node(store, name, value, parent=None):
    n = Node(id=uuid.uuid4(), parent=parent, name=name, value=value)
    store.add(n)
    return n


def _build_lamp():
    """「灯」词条树：名词，完整属性结构。"""
    store = Store()

    # 引用目标（别的词条，简化成一个节点）
    guangyuan = _node(store, "光源", (codec.EMPTY, None))
    dengguo = _node(store, "灯火", (codec.EMPTY, None))
    zhan = _node(store, "盏", (codec.EMPTY, None))
    zhi = _node(store, "只", (codec.EMPTY, None))

    lamp = _node(store, "灯", (codec.EMPTY, None))
    _node(store, "词形", (codec.TEXT, "灯"), parent=lamp.id)
    ciyi = _node(store, "词义", (codec.EMPTY, None), parent=lamp.id)

    yx01 = _node(store, "01", (codec.EMPTY, None), parent=ciyi.id)
    _node(store, "序号", (codec.TEXT, "01"), parent=yx01.id)
    _node(store, "读音", (codec.TEXT, "dēng"), parent=yx01.id)
    _node(store, "释义", (codec.TEXT, "照明或做其他用途的发光器具"), parent=yx01.id)
    liju = _node(store, "例句", (codec.EMPTY, None), parent=yx01.id)
    _node(store, "", (codec.TEXT, "床头柜上有一盏灯。"), parent=liju.id)
    _node(store, "", (codec.TEXT, "路灯在傍晚自动亮起。"), parent=liju.id)
    cixing = _node(store, "词性", (codec.EMPTY, None), parent=yx01.id)
    mingci = _node(store, "名词", (codec.EMPTY, None), parent=cixing.id)
    _node(store, "类属层级", (codec.TEXT, "类"), parent=mingci.id)
    _node(store, "语法子类", (codec.TEXT, "普通名词"), parent=mingci.id)
    _node(store, "实体性", (codec.TEXT, "实体"), parent=mingci.id)
    _node(store, "父类", (codec.REFERENCE, guangyuan.id), parent=mingci.id)
    liangci = _node(store, "量词", (codec.EMPTY, None), parent=mingci.id)
    _node(store, "", (codec.REFERENCE, zhan.id), parent=liangci.id)
    _node(store, "", (codec.REFERENCE, zhi.id), parent=liangci.id)
    lingyu = _node(store, "领域", (codec.EMPTY, None), parent=yx01.id)
    _node(store, "", (codec.TEXT, "通用"), parent=lingyu.id)
    yujing = _node(store, "语境", (codec.EMPTY, None), parent=yx01.id)
    _node(store, "", (codec.TEXT, "日常"), parent=yujing.id)
    guanlian = _node(store, "关联", (codec.EMPTY, None), parent=yx01.id)
    _node(store, "近义", (codec.REFERENCE, dengguo.id), parent=guanlian.id)

    return store, lamp, guangyuan, dengguo


def _build_baofu():
    """「包袱」词条：两个词义（数组），带词素引用。"""
    store = Store()

    bao = _node(store, "包", (codec.EMPTY, None))
    fu = _node(store, "袱", (codec.EMPTY, None))
    baoguo = _node(store, "包裹", (codec.EMPTY, None))
    xingnang = _node(store, "行囊", (codec.EMPTY, None))

    baofu = _node(store, "包袱", (codec.EMPTY, None))
    _node(store, "词形", (codec.TEXT, "包袱"), parent=baofu.id)
    ciyi = _node(store, "词义", (codec.EMPTY, None), parent=baofu.id)

    yx01 = _node(store, "01", (codec.EMPTY, None), parent=ciyi.id)
    _node(store, "释义", (codec.TEXT, "用布包裹起来的包儿"), parent=yx01.id)
    cisu = _node(store, "词素", (codec.EMPTY, None), parent=yx01.id)
    _node(store, "", (codec.REFERENCE, bao.id), parent=cisu.id)
    _node(store, "", (codec.REFERENCE, fu.id), parent=cisu.id)
    guanlian = _node(store, "关联", (codec.EMPTY, None), parent=yx01.id)
    _node(store, "同义", (codec.REFERENCE, baoguo.id), parent=guanlian.id)
    _node(store, "近义", (codec.REFERENCE, xingnang.id), parent=guanlian.id)

    yx02 = _node(store, "02", (codec.EMPTY, None), parent=ciyi.id)
    _node(store, "释义", (codec.TEXT, "比喻某种负担或思想压力"), parent=yx02.id)

    return store, baofu


# —— 基础操作 ——

def test_get_and_roots():
    store, lamp, guangyuan, dengguo = _build_lamp()
    assert store.get(lamp.id) is lamp
    root_names = {n.name for n in store.roots()}
    assert root_names >= {"灯", "光源", "灯火", "盏", "只"}


def test_children_as_array_order():
    # 数组解读：子节点按追加序
    store, lamp, _, _ = _build_lamp()
    assert [n.name for n in store.children(lamp)] == ["词形", "词义"]


def test_map_interpretation():
    # map 解读：节点名 = key
    store, lamp, _, _ = _build_lamp()
    ciyi = store.child_by_name(lamp, "词义")
    yx01 = store.child_by_name(ciyi, "01")
    cixing = store.child_by_name(yx01, "词性")
    assert set(store.as_map(cixing).keys()) == {"名词"}


def test_array_of_sentences():
    # 例句 = 数组（无名字的子节点，按顺序）
    store, lamp, _, _ = _build_lamp()
    ciyi = store.child_by_name(lamp, "词义")
    yx01 = store.child_by_name(ciyi, "01")
    liju = store.child_by_name(yx01, "例句")
    sentences = [n.value[1] for n in store.children(liju)]
    assert sentences == ["床头柜上有一盏灯。", "路灯在傍晚自动亮起。"]


def test_multiple_entries_array():
    # 「包袱」两个词义 = 数组
    store, baofu = _build_baofu()
    ciyi = store.child_by_name(baofu, "词义")
    assert [n.name for n in store.children(ciyi)] == ["01", "02"]


def test_resolve_reference():
    # 枚举/关联解读：引用 → 目标节点
    store, lamp, guangyuan, _ = _build_lamp()
    ciyi = store.child_by_name(lamp, "词义")
    yx01 = store.child_by_name(ciyi, "01")
    cixing = store.child_by_name(yx01, "词性")
    mingci = store.child_by_name(cixing, "名词")
    fulei = store.child_by_name(mingci, "父类")
    assert store.resolve(fulei) is guangyuan


def test_walk_traversal():
    store, lamp, _, _ = _build_lamp()
    names = [n.name for n in walk(store, lamp)]
    assert names[0] == "灯"
    assert "释义" in names and "近义" in names


def test_encode_decode_roundtrip():
    store, _, _, _ = _build_lamp()
    store2 = Store.decode(store.encode())
    assert len(store2) == len(store)
    for n in store.nodes():
        n2 = store2.get(n.id)
        assert n2 is not None
        assert codec.encode_node(n2) == codec.encode_node(n)


def test_save_load_roundtrip(tmp_path):
    store, lamp, _, _ = _build_lamp()
    path = tmp_path / "lamp.xirang"
    store.save(path)
    store2 = Store.load(path)
    assert len(store2) == len(store)
    assert store2.get(lamp.id).name == "灯"


def test_file_has_magic_and_header(tmp_path):
    # 文件 = 魔数 XRNG + 版本字节 + 头长 + 头文本 + 节点
    store, _, _, _ = _build_lamp()
    path = tmp_path / "lamp.xirang"
    store.save(path)
    raw = path.read_bytes()
    assert raw[:4] == b"XRNG"
    assert raw[4] == 1  # 格式版本
    n = int.from_bytes(raw[5:9], "big")
    header = raw[9:9 + n].decode("utf-8")
    assert "XiRang Tree" in header
    assert "node_id" in header
    assert header.rstrip().endswith("Read nodes until end of file.")


def test_load_bad_magic_raises(tmp_path):
    store, _, _, _ = _build_lamp()
    path = tmp_path / "bad.xirang"
    store.save(path)
    raw = bytearray(path.read_bytes())
    raw[:4] = b"XXXX"
    path.write_bytes(bytes(raw))
    try:
        Store.load(path)
        assert False, "坏魔数应该抛 F001"
    except ValueError as e:
        assert "F001" in str(e)


# —— 时间解读 ——

def test_as_time_from_integer():
    n = Node(id=uuid.uuid4(), parent=None, name="发生时刻",
             value=(codec.INT, 1704067200))
    assert as_time(n) == "2024-01-01T00:00:00+00:00"


def test_as_time_from_text():
    n = Node(id=uuid.uuid4(), parent=None, name="出生日期",
             value=(codec.TEXT, "1990-01-01"))
    assert as_time(n) == "1990-01-01"


def test_as_time_none_for_non_time():
    n = Node(id=uuid.uuid4(), parent=None, name="灯",
             value=(codec.EMPTY, None))
    assert as_time(n) is None


# —— 辅助节点（@ 前缀）——

def test_skip_aux():
    store = Store()
    lamp = _node(store, "灯", (codec.EMPTY, None))
    _node(store, "词形", (codec.TEXT, "灯"), lamp.id)
    _node(store, "@note", (codec.TEXT, "for the reader"), lamp.id)

    # 默认包含 @
    assert [n.name for n in store.children(lamp)] == ["词形", "@note"]
    # skip_aux 跳过 @
    assert [n.name for n in store.children(lamp, skip_aux=True)] == ["词形"]
    # aux 只取 @
    assert [n.name for n in store.aux(lamp)] == ["@note"]
    # walk：默认包含，skip_aux 跳过
    assert any(n.name == "@note" for n in walk(store, lamp))
    assert not any(n.name == "@note" for n in walk(store, lamp, skip_aux=True))


# —— 查补全 + 写操作（改/删）——

def test_new_and_parent():
    store = Store()
    lamp = store.new(None, "灯", (codec.EMPTY, None))
    cixing = store.new(lamp, "词形", (codec.TEXT, "灯"))
    assert store.parent(cixing) is lamp
    assert store.parent(lamp) is None
    # new 默认挂 @created
    assert [c.name for c in store.children(lamp) if c.name == "@created"] == ["@created"]


def test_references_to():
    store = Store()
    guangyuan = _node(store, "光源", (codec.EMPTY, None))
    deng = _node(store, "灯", (codec.EMPTY, None))
    _node(store, "父类", (codec.REFERENCE, guangyuan.id), deng.id)
    _node(store, "父类2", (codec.REFERENCE, guangyuan.id), deng.id)
    assert {n.name for n in store.references_to(guangyuan)} == {"父类", "父类2"}


def test_subtree_and_find():
    store = Store()
    lamp = _node(store, "灯", (codec.EMPTY, None))
    _node(store, "词形", (codec.TEXT, "灯"), lamp.id)
    _node(store, "词频", (codec.INT, 2046), lamp.id)
    assert [n.name for n in store.subtree(lamp)] == ["灯", "词形", "词频"]
    assert store.find(name="词频")[0].value == (codec.INT, 2046)
    assert len(store.find(value=(codec.TEXT, "灯"))) == 1  # 只有「词形」的值是"灯"


def test_update_keeps_id_and_history():
    store = Store()
    guangyuan = _node(store, "光源", (codec.TEXT, "能发光的物体"))
    old_id = guangyuan.id
    store.update(guangyuan, (codec.TEXT, "新的定义"))
    assert guangyuan.id == old_id                    # 编号不变
    assert guangyuan.value == (codec.TEXT, "新的定义")
    history = store.child_by_name(guangyuan, "@history")
    assert history is not None
    snaps = store.children(history)
    assert len(snaps) == 1
    assert snaps[0].value == (codec.TEXT, "能发光的物体")   # 旧值留痕
    assert [c.name for c in store.children(snaps[0])] == ["@replaced"]


def test_update_reference_follows():
    store = Store()
    guangyuan = _node(store, "光源", (codec.TEXT, "能发光的物体"))
    deng = _node(store, "灯", (codec.EMPTY, None))
    fulei = _node(store, "父类", (codec.REFERENCE, guangyuan.id), deng.id)
    store.update(guangyuan, (codec.TEXT, "新的定义"))
    # 引用仍指向原编号，resolve 自动看到新值
    target = store.resolve(fulei)
    assert target is guangyuan
    assert target.value == (codec.TEXT, "新的定义")


def test_remove_clears_in_place_with_history():
    # 与现行规范 / Rust 一致：删 = 置空留槽位 + 本节点 @history（@replaced），不再搬到父节点下
    store = Store()
    deng = _node(store, "灯", (codec.EMPTY, None))
    guangyuan = _node(store, "光源", (codec.TEXT, "能发光的物体"), deng.id)
    store.remove(guangyuan)
    # 节点留在原位、编号保留、父子关系不变，名字/值被清空
    assert store.get(guangyuan.id) is guangyuan
    assert guangyuan.parent == deng.id
    assert guangyuan.name == ""
    assert guangyuan.value == (codec.EMPTY, None)
    # 旧值进了「自己」的 @history（快照 + @replaced）
    history = store.child_by_name(guangyuan, "@history")
    assert history is not None
    snap = store.child_by_name(history, "光源")
    assert snap is not None
    assert snap.value == (codec.TEXT, "能发光的物体")
    assert [c.name for c in store.children(snap)] == ["@replaced"]
