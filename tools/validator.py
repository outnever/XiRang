"""息壤校验器（v1.0）。

对一批节点做**结构校验**，返回错误列表。错误码清单见 `errors/错误列表.md`。

本版只做结构校验（编号 / 类型 / 值格式 / 父边 / 引用），
不做版本、顺序、前缀通道（那些仍是「参考草案」，未最终确定）。

v1.0 适用的错误码：

    E001 编号缺失        节点编号缺失或为 nil（全 0）
    E002 编号冲突        两个节点编号相同
    E003 编号格式非法    编号不是合法 UUID（防御性检查）
    E005 父节点自指      节点的父节点 = 自己
    E006 父节点成环      沿父边走回到自己
    E008 类型标记非法    类型标记不在 0-6
    E009 值内容非法      值内容与其大类不符
    E011 父节点断裂      父节点指向不存在的节点
    R001 引用断裂        引用指向不存在的节点
"""

import uuid
from dataclasses import dataclass

from tools import codec


@dataclass
class Error:
    code: str      # 错误码，如 "R001"
    node_id: str   # 相关节点的编号
    message: str   # 描述


def _check_value(n: codec.Node):
    """E008 / E009：类型标记 + 值格式。"""
    tag, data = n.value
    if tag not in codec.TAG_NAMES:
        return Error("E008", str(n.id), f"类型标记非法：{tag}")

    if tag == codec.EMPTY:
        if data is not None:
            return Error("E009", str(n.id), "空类型的值应为空（None）")
    elif tag == codec.INT:
        if not (isinstance(data, int) and not isinstance(data, bool)):
            return Error("E009", str(n.id), "整数类型的值不是整数")
    elif tag == codec.FLOAT:
        if not isinstance(data, float):
            return Error("E009", str(n.id), "浮点类型的值不是小数")
    elif tag == codec.BOOL:
        if not isinstance(data, bool):
            return Error("E009", str(n.id), "布尔类型的值不是真/假")
    elif tag == codec.TEXT:
        if not isinstance(data, str):
            return Error("E009", str(n.id), "文本类型的值不是字符串")
    elif tag == codec.REFERENCE:
        if not isinstance(data, uuid.UUID):
            return Error("E009", str(n.id), "引用类型的值不是 UUID")
    elif tag == codec.BLOB:
        if not isinstance(data, bytes):
            return Error("E009", str(n.id), "二进制块类型的值不是字节")
    return None


def _check_cycles(nodes, index):
    """E006：父边成环（长度 ≥ 2；自指已在 E005 单独报）。

    同一个环只报一次（按环内最小编号的那次报），避免日志噪音随环长线性增长。
    """
    errors = []
    seen_cycle = set()  # 已报过的环内编号
    for start in nodes:
        if start.parent is None or start.parent == start.id:
            continue
        if start.id in seen_cycle:
            continue
        path = [start.id]
        seen = {start.id}
        cur = index.get(start.parent)
        cycle = None
        while cur is not None:
            if cur.id == start.id:
                cycle = list(path)
                break
            if cur.id in seen:
                break
            seen.add(cur.id)
            path.append(cur.id)
            cur = index.get(cur.parent) if cur.parent is not None else None
        if cycle is not None:
            seen_cycle.update(cycle)
            # 只在这个环里「第一个被遍历到」的节点上报一次；其余成员后面会被 seen_cycle 跳过
            errors.append(Error("E006", str(start.id), "父节点成环"))
    return errors


def validate(nodes) -> list:
    """校验一批节点，返回 Error 列表。nodes: list[Node]。"""
    errors = []
    index = {}  # UUID -> Node（索引：按编号寻址）
    seen = {}   # UUID -> Node（查编号冲突）

    # 第一遍：单节点校验 + 建索引 + 查编号冲突
    for n in nodes:
        if n.id == codec.NIL_UUID:
            errors.append(Error("E001", str(n.id), "节点编号缺失（为 nil）"))
        if not isinstance(n.id, uuid.UUID):
            errors.append(Error("E003", str(n.id), "编号不是合法 UUID"))
        if n.id in seen:
            errors.append(Error("E002", str(n.id), "编号冲突（与另一节点相同）"))
        else:
            seen[n.id] = n
        if n.parent is not None and n.parent == n.id:
            errors.append(Error("E005", str(n.id), "父节点指向自己"))
        err = _check_value(n)
        if err:
            errors.append(err)
        index[n.id] = n

    # 第二遍：树级校验（父边 + 引用）
    for n in nodes:
        if n.parent is not None and n.parent not in index:
            errors.append(Error("E011", str(n.id), f"父节点断裂：{n.parent} 不存在"))
        tag, data = n.value
        if tag == codec.REFERENCE and data not in index:
            errors.append(Error("R001", str(n.id), f"引用断裂：目标 {data} 不存在"))

    # 父节点成环
    errors.extend(_check_cycles(nodes, index))

    return errors
