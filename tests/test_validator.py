"""validator 的测试：结构校验能查出对应的错误码。"""
import uuid

from tools import codec
from tools.codec import Node
from tools.validator import validate


def _codes(nodes):
    return [e.code for e in validate(nodes)]


def test_valid_tree_no_errors():
    a, b = uuid.uuid4(), uuid.uuid4()
    nodes = [
        Node(id=a, parent=None, name="灯", value=(codec.EMPTY, None)),
        Node(id=b, parent=a, name="释义", value=(codec.TEXT, "照明或发热的器具")),
    ]
    assert validate(nodes) == []


def test_E001_nil_id():
    n = Node(id=codec.NIL_UUID, parent=None, name="", value=(codec.EMPTY, None))
    assert "E001" in _codes([n])


def test_E002_duplicate_id():
    x = uuid.uuid4()
    nodes = [
        Node(id=x, parent=None, name="a", value=(codec.EMPTY, None)),
        Node(id=x, parent=None, name="b", value=(codec.EMPTY, None)),
    ]
    assert "E002" in _codes(nodes)


def test_E005_self_parent():
    x = uuid.uuid4()
    n = Node(id=x, parent=x, name="", value=(codec.EMPTY, None))
    codes = _codes([n])
    assert "E005" in codes
    assert "E006" not in codes  # 自指不算「环」，避免重复报


def test_E006_parent_cycle():
    a, b = uuid.uuid4(), uuid.uuid4()
    nodes = [
        Node(id=a, parent=b, name="", value=(codec.EMPTY, None)),
        Node(id=b, parent=a, name="", value=(codec.EMPTY, None)),
    ]
    assert "E006" in _codes(nodes)


def test_E008_bad_tag():
    n = Node(id=uuid.uuid4(), parent=None, name="", value=(99, None))
    assert "E008" in _codes([n])


def test_E009_value_type_mismatch():
    n = Node(id=uuid.uuid4(), parent=None, name="", value=(codec.INT, "不是整数"))
    assert "E009" in _codes([n])


def test_E011_dangling_parent():
    ghost = uuid.uuid4()
    n = Node(id=uuid.uuid4(), parent=ghost, name="", value=(codec.EMPTY, None))
    assert "E011" in _codes([n])


def test_R001_dangling_reference():
    ghost = uuid.uuid4()
    n = Node(id=uuid.uuid4(), parent=None, name="", value=(codec.REFERENCE, ghost))
    assert "R001" in _codes([n])


def test_R001_valid_reference_no_error():
    a, b = uuid.uuid4(), uuid.uuid4()
    nodes = [
        Node(id=a, parent=None, name="灯", value=(codec.EMPTY, None)),
        Node(id=b, parent=a, name="事件主体", value=(codec.REFERENCE, a)),
    ]
    assert validate(nodes) == []
