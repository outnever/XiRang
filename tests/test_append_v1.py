"""append-v1（修订v1）：折叠读 + E002 语义（Python 参考实现）。

与 Rust 实现（rust/core/src/tree.rs、rust/core/src/validator.rs、rust/core/tests/append_v1.rs）
语义对齐：同一编号多记录 = 修订，读时取最后一条；声明了 `@protocol = append-v1`
的根下不算编号冲突。
"""

import uuid

from tools import codec, tree, validator


def n(nid, parent, name, value):
    return codec.Node(id=nid, parent=parent, name=name, value=value)


def ids(k=4):
    return [uuid.UUID(int=i + 1) for i in range(k)]


def marker(store, root_id):
    """在根下挂 `@protocol = append-v1`。"""
    store.add(n(uuid.uuid4(), root_id, "@protocol", (codec.TEXT, tree.PROTOCOL_APPEND)))


def test_fold_keeps_last_record_and_first_position():
    a, b, c, _ = ids()
    s = tree.Store()
    s.add(n(a, None, "根", (codec.EMPTY, None)))
    s.add(n(b, a, "甲", (codec.TEXT, "旧")))
    s.add(n(c, a, "乙", (codec.EMPTY, None)))
    s.add(n(b, a, "甲", (codec.TEXT, "新")))

    folded = s.fold()
    assert len(folded) == 3
    assert folded.get(b).value == (codec.TEXT, "新")
    assert [x.id for x in folded.nodes()] == [a, b, c], "顺序按首次出现位置"
    assert len(folded.children(folded.get(a))) == 2, "孩子不重复"


def test_fold_handles_delete_and_parent_move():
    a, b, c, _ = ids()
    s = tree.Store()
    s.add(n(a, None, "根", (codec.EMPTY, None)))
    s.add(n(b, None, "另一个根", (codec.EMPTY, None)))
    s.add(n(c, a, "丙", (codec.TEXT, "值")))
    s.add(n(c, a, "", (codec.EMPTY, None)))
    s.add(n(c, b, "丙", (codec.TEXT, "搬家")))

    folded = s.fold()
    assert folded.children(folded.get(a)) == []
    assert [x.id for x in folded.children(folded.get(b))] == [c]
    assert folded.get(c).value == (codec.TEXT, "搬家")


def test_validate_view_allows_declared_revisions():
    a, b, _, _ = ids()
    s = tree.Store()
    s.add(n(a, None, "根", (codec.EMPTY, None)))
    marker(s, a)
    s.add(n(b, a, "词形", (codec.TEXT, "灯")))
    s.add(n(b, a, "词形", (codec.TEXT, "灯（改）")))

    assert validator.validate_view(s) == [], "声明了修订协议 → 重复编号不是冲突"
    assert any(e.code == "E002" for e in validator.validate(s.nodes())), "原始视角仍看得到重复"


def test_undeclared_duplicates_still_conflict():
    a, b, _, _ = ids()
    s = tree.Store()
    s.add(n(a, None, "根", (codec.EMPTY, None)))
    s.add(n(b, a, "词形", (codec.TEXT, "灯")))
    s.add(n(b, a, "词形", (codec.TEXT, "灯（改）")))
    assert any(e.code == "E002" for e in validator.validate_view(s))


def test_protocol_roots_and_root_of():
    a, b, _, _ = ids()
    s = tree.Store()
    s.add(n(a, None, "根", (codec.EMPTY, None)))
    s.add(n(b, a, "甲", (codec.EMPTY, None)))
    marker(s, a)

    assert s.protocol_roots(tree.PROTOCOL_APPEND) == [a]
    assert s.root_of(b) == a
    assert s.root_of(a) == a
