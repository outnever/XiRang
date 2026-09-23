"""格式转换（依赖内核 v1.0）：Store（一批节点）↔ JSON / XML / YAML / Markdown。

- JSON / XML / YAML：无损往返（round-trip）。
- Markdown：有损，只导出（`to_md`），无导入。
- 二进制块：Base64 内联（默认）或外置到文件（传入 `blob_dir`）；导出时默认补 `@fingerprint` 辅助节点（可 `add_fingerprint=False` 关闭）。
- 减量（slim）：`to_json_slim` / `from_json_slim` / `to_yaml_slim` / `from_yaml_slim`，名字当键、有损。

值统一用显式类型标记表示：`{"type": <名>, "value": <内容>}`。
类型名与文件头文本一致：empty / integer / float / boolean / text / reference / blob。
"""

import base64
import hashlib
import json
import os
import uuid
import xml.etree.ElementTree as ET

import yaml

from tools import codec
from tools.tree import Store

KERNEL = "1.0"

# 大类标记 ↔ 类型名
TYPE_NAMES = {
    codec.EMPTY: "empty",
    codec.INT: "integer",
    codec.FLOAT: "float",
    codec.BOOL: "boolean",
    codec.TEXT: "text",
    codec.REFERENCE: "reference",
    codec.BLOB: "blob",
}
TYPE_TAGS = {v: k for k, v in TYPE_NAMES.items()}


# ---------------------------------------------------------------------------
# 值 ↔ 字典
# ---------------------------------------------------------------------------

def _encode_value(tag, data, blob_dir):
    """值 (tag, data) → 字典。"""
    if tag == codec.EMPTY:
        return {"type": "empty", "value": None}
    if tag == codec.BLOB:
        if blob_dir is None:
            return {"type": "blob",
                    "value": base64.b64encode(data).decode("ascii"),
                    "encoding": "base64"}
        os.makedirs(blob_dir, exist_ok=True)
        fname = "blob_" + hashlib.sha256(data).hexdigest() + ".bin"
        with open(os.path.join(blob_dir, fname), "wb") as f:
            f.write(data)
        return {"type": "blob", "ref": fname}
    if tag == codec.REFERENCE:
        return {"type": TYPE_NAMES[tag], "value": str(data)}
    return {"type": TYPE_NAMES[tag], "value": data}


def _decode_value(d, blob_dir):
    """字典 → 值 (tag, data)。schema 不变量违规报 C 码（见 errors/错误列表.md）。"""
    name = d.get("type")
    if name not in TYPE_TAGS:
        raise ValueError(f"C001：类型名非法（{name!r}）")
    tag = TYPE_TAGS[name]
    if tag == codec.EMPTY:
        return tag, None
    if tag == codec.BLOB:
        if "ref" in d:
            path = os.path.join(blob_dir or "", d["ref"])
            if not os.path.isfile(path):
                raise ValueError(f"C004：外置文件缺失（{d['ref']}）")
            with open(path, "rb") as f:
                return tag, f.read()
        try:
            return tag, base64.b64decode(d["value"], validate=True)
        except Exception:
            raise ValueError("C003：二进制块编码非法（Base64 解码失败）")
    if tag == codec.REFERENCE:
        try:
            return tag, uuid.UUID(d["value"])
        except (ValueError, AttributeError, TypeError):
            raise ValueError(f"C002：UUID 非法（{d.get('value')!r}）")
    return tag, d["value"]


def _value_to_text(d):
    """字典 → XML 文本内容（native 类型转字符串）。"""
    if d["type"] == "empty":
        return ""  # 与 Rust 一致：空值写空内容，不写 "None"
    if d["type"] in ("boolean",):
        return "true" if d["value"] else "false"
    return str(d["value"])


def _text_to_value(name, text):
    """XML 文本内容 → native 值。"""
    if name == "integer":
        return int(text)
    if name == "float":
        return float(text)
    if name == "boolean":
        return text == "true"
    if name == "text":
        return text or ""  # 空元素：.text 为 None → 空串
    return text  # reference：字符串，在 _decode_value 里再转 UUID


# ---------------------------------------------------------------------------
# Store ↔ 扁平节点字典列表
# ---------------------------------------------------------------------------

def _store_to_flat(store, blob_dir=None):
    out = []
    for n in store.nodes():
        out.append({
            "id": str(n.id),
            "parent": str(n.parent) if n.parent is not None else None,
            "name": n.name,
            "value": _encode_value(n.value[0], n.value[1], blob_dir),
        })
    return out


def _node_from_dict(d, parent, blob_dir):
    """节点字典 → Node。字段缺失报 C005，UUID 非法报 C002。"""
    for field in ("id", "name", "value"):
        if field not in d:
            raise ValueError(f"C005：节点字段缺失（{field}）")
    try:
        nid = uuid.UUID(d["id"])
    except (ValueError, AttributeError, TypeError):
        raise ValueError(f"C002：UUID 非法（id={d['id']!r}）")
    tag, data = _decode_value(d["value"], blob_dir)
    return codec.Node(id=nid, parent=parent, name=d["name"], value=(tag, data))


def _flat_to_store(nodes, blob_dir=None):
    store = Store()
    for d in nodes:
        parent = None
        if d.get("parent") is not None:
            try:
                parent = uuid.UUID(d["parent"])
            except (ValueError, AttributeError, TypeError):
                raise ValueError(f"C002：UUID 非法（parent={d['parent']!r}）")
        store.add(_node_from_dict(d, parent, blob_dir))
    return store


# ---------------------------------------------------------------------------
# Store ↔ 嵌套节点字典（roots + children）
# ---------------------------------------------------------------------------

def _node_to_nested(store, node, blob_dir):
    d = {
        "id": str(node.id),
        "name": node.name,
        "value": _encode_value(node.value[0], node.value[1], blob_dir),
    }
    children = store.children(node)
    if children:
        d["children"] = [_node_to_nested(store, c, blob_dir) for c in children]
    return d


def _nested_to_store(roots, blob_dir=None):
    store = Store()

    def build(d, parent):
        n = _node_from_dict(d, parent, blob_dir)
        store.add(n)
        for c in d.get("children", []):
            build(c, n.id)

    for r in roots:
        build(r, None)
    return store


# ---------------------------------------------------------------------------
# 指纹补充（导出时给二进制块补 @fingerprint 辅助节点）
# ---------------------------------------------------------------------------

def _with_fingerprints(store):
    """返回补了 `@fingerprint` 辅助节点的 Store 视图（幂等：已有则不重复补）。"""
    extra = []
    for n in store.nodes():
        if n.value[0] == codec.BLOB and not any(c.name == "@fingerprint" for c in store.children(n)):
            extra.append(codec.Node(
                id=uuid.uuid4(),
                parent=n.id,
                name="@fingerprint",
                value=(codec.TEXT, hashlib.sha256(n.value[1]).hexdigest()),
            ))
    if not extra:
        return store
    return Store(store.nodes() + extra)


# ---------------------------------------------------------------------------
# JSON
# ---------------------------------------------------------------------------

def to_json(store, blob_dir=None, nested=False, add_fingerprint=False):
    if add_fingerprint:
        store = _with_fingerprints(store)
    if nested:
        obj = {"format": "xirang", "kernel": KERNEL,
               "roots": [_node_to_nested(store, r, blob_dir) for r in store.roots()]}
    else:
        obj = {"format": "xirang", "kernel": KERNEL, "nodes": _store_to_flat(store, blob_dir)}
    return json.dumps(obj, ensure_ascii=False, indent=2)


def from_json(text, blob_dir=None):
    d = json.loads(text)
    if "roots" in d:
        return _nested_to_store(d["roots"], blob_dir)
    return _flat_to_store(d["nodes"], blob_dir)


# ---------------------------------------------------------------------------
# XML
# ---------------------------------------------------------------------------

def to_xml(store, blob_dir=None, add_fingerprint=False):
    if add_fingerprint:
        store = _with_fingerprints(store)
    root = ET.Element("xirang", {"kernel": KERNEL})
    for d in _store_to_flat(store, blob_dir):
        el = ET.SubElement(root, "node", {"id": d["id"]})
        if d["parent"] is not None:
            el.set("parent", d["parent"])
        name = ET.SubElement(el, "name")
        name.text = d["name"]
        v = d["value"]
        value = ET.SubElement(el, "value", {"type": v["type"]})
        if "ref" in v:
            value.set("ref", v["ref"])
        elif "encoding" in v:
            value.set("encoding", v["encoding"])
            value.text = v["value"]
        else:
            value.text = _value_to_text(v)
    return ET.tostring(root, encoding="unicode")


def from_xml(text, blob_dir=None):
    root = ET.fromstring(text)
    nodes = []
    for el in root.findall("node"):
        v_el = el.find("value")
        v = {"type": v_el.get("type")}
        if v_el.get("ref"):
            v["ref"] = v_el.get("ref")
        elif v_el.get("encoding"):
            v["value"] = v_el.text
            v["encoding"] = v_el.get("encoding")
        else:
            v["value"] = _text_to_value(v_el.get("type"), v_el.text)
        nodes.append({
            "id": el.get("id"),
            "parent": el.get("parent"),
            "name": el.find("name").text or "",
            "value": v,
        })
    return _flat_to_store(nodes, blob_dir)


# ---------------------------------------------------------------------------
# YAML
# ---------------------------------------------------------------------------

def to_yaml(store, blob_dir=None, add_fingerprint=False):
    if add_fingerprint:
        store = _with_fingerprints(store)
    obj = {"format": "xirang", "kernel": KERNEL, "nodes": _store_to_flat(store, blob_dir)}
    return yaml.safe_dump(obj, allow_unicode=True, sort_keys=False)


def from_yaml(text, blob_dir=None):
    d = yaml.safe_load(text)
    return _flat_to_store(d["nodes"], blob_dir)


# ---------------------------------------------------------------------------
# Markdown（有损，只导出）
# ---------------------------------------------------------------------------

def _md_value(store, node):
    tag, data = node.value
    if tag == codec.TEXT:
        return data
    if tag in (codec.INT, codec.FLOAT, codec.BOOL):
        return str(data)
    if tag == codec.REFERENCE:
        target = store.get(data)
        return f"→ {target.name}" if target else f"→ {data}"
    if tag == codec.BLOB:
        return f"[二进制块] {len(data)} 字节"
    return ""


def _md_node(store, node, lines, level):
    lines.append("")
    lines.append(f"{'#' * level} {node.name or '(未命名)'}")
    v = _md_value(store, node)
    if v:
        lines.append("")
        lines.append(v)
    children = store.children(node)
    if children:
        lines.append("")
        for c in children:
            if store.children(c):
                _md_node(store, c, lines, level + 1)
            else:
                lines.append(f"- **{c.name or '(未命名)'}**: {_md_value(store, c)}")
    lines.append("")


def to_md(store, add_fingerprint=True):
    if add_fingerprint:
        store = _with_fingerprints(store)
    lines = ["# 息壤树"]
    for root in store.roots():
        _md_node(store, root, lines, level=2)
    return "\n".join(lines).rstrip() + "\n"


# ---------------------------------------------------------------------------
# 减量导出（slim）：名字当键，有损，只留层级 + 值内容
# ---------------------------------------------------------------------------

def _slim_value(store, tag, data):
    """值 → slim 标量。"""
    if tag in (codec.TEXT, codec.INT, codec.FLOAT, codec.BOOL):
        return data
    if tag == codec.REFERENCE:
        target = store.get(data)
        return target.name if target else str(data)  # 引用 → 目标名
    if tag == codec.BLOB:
        return f"[二进制块 {len(data)} 字节]"
    return ""


def _slim_children(store, node):
    """节点的非 `@` 辅助子节点。"""
    return [c for c in store.children(node) if not c.name.startswith("@")]


def _slim_children_repr(store, children):
    """子节点列表 → map（对象，全有名）或 list（数组，含无名）。"""
    if all(c.name for c in children):
        return {c.name: _slim_content(store, c) for c in children}
    out = []
    for c in children:
        content = _slim_content(store, c)
        out.append({c.name: content} if c.name else content)
    return out


def _slim_content(store, node):
    """节点 → slim 内容（标量 / 对象 / 数组 / {value, children}）。"""
    children = _slim_children(store, node)
    tag, data = node.value
    value = _slim_value(store, tag, data)
    if not children:
        return value
    child_repr = _slim_children_repr(store, children)
    if tag == codec.EMPTY:
        return child_repr  # 容器（空值）
    return {"value": value, "children": child_repr}  # 值 + 子节点


def _slim_roots(store):
    return [{r.name: _slim_content(store, r)} for r in store.roots()]


def to_json_slim(store):
    return json.dumps({"roots": _slim_roots(store)}, ensure_ascii=False, indent=2)


def to_yaml_slim(store):
    return yaml.safe_dump({"roots": _slim_roots(store)}, allow_unicode=True, sort_keys=False)


# —— 减量导入 ——

def _slim_scalar_to_value(x):
    """slim 标量 → (tag, data)。"""
    if x is None:
        return codec.EMPTY, None
    if isinstance(x, bool):
        return codec.BOOL, x
    if isinstance(x, int):
        return codec.INT, x
    if isinstance(x, float):
        return codec.FLOAT, x
    if isinstance(x, str):
        return codec.TEXT, x
    return codec.TEXT, str(x)


def _slim_add(store, name, tag, data, parent):
    n = codec.Node(id=uuid.uuid4(), parent=parent, name=name, value=(tag, data))
    store.add(n)
    return n


def _slim_build_children(store, child_repr, parent):
    """children 表示（map 或 list）→ 建子节点。"""
    if isinstance(child_repr, dict):
        for cname, ccontent in child_repr.items():
            _slim_build(store, cname, ccontent, parent)
    else:
        for elem in child_repr:
            _slim_build_elem(store, elem, parent)


def _slim_build_elem(store, elem, parent):
    """数组元素 → 子节点：标量（无名）或 {name: content}（有名）。"""
    if isinstance(elem, dict) and len(elem) == 1:
        name, content = next(iter(elem.items()))
        _slim_build(store, name, content, parent)
    else:
        _slim_build(store, "", elem, parent)


def _slim_build(store, name, content, parent):
    """(name, content) → 节点。"""
    if isinstance(content, list):
        n = _slim_add(store, name, codec.EMPTY, None, parent)
        for elem in content:
            _slim_build_elem(store, elem, n.id)
        return n
    if isinstance(content, dict):
        if set(content.keys()) == {"value", "children"}:
            tag, data = _slim_scalar_to_value(content["value"])
            n = _slim_add(store, name, tag, data, parent)
            _slim_build_children(store, content["children"], n.id)
            return n
        n = _slim_add(store, name, codec.EMPTY, None, parent)
        for cname, ccontent in content.items():
            _slim_build(store, cname, ccontent, n.id)
        return n
    tag, data = _slim_scalar_to_value(content)
    return _slim_add(store, name, tag, data, parent)


def _slim_from(roots):
    store = Store()
    for r in roots:
        for name, content in r.items():
            _slim_build(store, name, content, None)
    return store


def from_json_slim(text):
    return _slim_from(json.loads(text).get("roots", []))


def from_yaml_slim(text):
    return _slim_from(yaml.safe_load(text).get("roots", []))
