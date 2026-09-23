"""息壤 CLI（MVP，P0）：xr 命令，复用 tools/ 的编解码、树处理与校验。

用法（在仓库根目录执行）：

    python3 -m tools.cli info <file>                 # 文件摘要
    python3 -m tools.cli tree <file> [--node <id>] [--skip-aux]   # 缩进树
    python3 -m tools.cli validate <file>             # E/R 校验

说明：后续 M2 将移植为 Rust 单二进制 `xr`；本文件是命令面的 Python 原型。
"""
import argparse
import sys
import uuid
from pathlib import Path

from tools import codec
from tools import tree
from tools import validator


# ---------------------------------------------------------------------------
# 工具函数
# ---------------------------------------------------------------------------

def _read_header(path: Path):
    """读文件头，返回 (format_version, header_text)。非法抛 ValueError。"""
    raw = path.read_bytes()
    if len(raw) < 9 or raw[:4] != b"XRNG":
        raise ValueError("F001：魔数非法（不是 XRNG 息壤文件）")
    version = raw[4]
    if version != 1:
        raise ValueError(f"F002：格式版本不支持（{version}）")
    n = int.from_bytes(raw[5:9], "big")
    if 9 + n > len(raw):
        raise ValueError("F003：头长非法")
    return version, raw[9:9 + n].decode("utf-8")


def _fmt_value(store: tree.Store, node) -> str | None:
    """把节点值格式化成可读文本；空值返回 None。"""
    tag, data = node.value
    if tag == codec.EMPTY:
        return None
    if tag == codec.INT:
        return str(data)
    if tag == codec.FLOAT:
        return str(data)
    if tag == codec.BOOL:
        return "true" if data else "false"
    if tag == codec.TEXT:
        return data
    if tag == codec.REFERENCE:
        target = store.get(data)
        return f"→ {target.name if target else data}"
    if tag == codec.BLOB:
        return f"[blob {len(data)} 字节]"
    return f"<tag{tag}>"


def _label(store: tree.Store, node) -> str:
    name = node.name
    val = _fmt_value(store, node)
    if val is None:
        return name if name else "(空节点)"
    if name:
        return f"{name} = {val}"
    return val  # 无名节点（数组元素），只显示值


# ---------------------------------------------------------------------------
# 命令
# ---------------------------------------------------------------------------

def cmd_info(args):
    path = Path(args.file)
    version, header = _read_header(path)
    store = tree.Store.load(path)
    first_line = header.strip().split("\n")[0] if header.strip() else "(空)"
    print(f"文件: {path}")
    print(f"格式版本: {version}")
    print(f"头文本: {len(header)} 字节，首行: {first_line}")
    print(f"节点数: {len(store)}")
    print(f"根节点数: {len(store.roots())}")
    return 0


def _print_tree(store: tree.Store, node, prefix="", is_last=True, skip_aux=False):
    children = store.children(node)
    if skip_aux:
        children = [c for c in children if not c.name.startswith("@")]
    connector = "" if prefix == "" else ("└─ " if is_last else "├─ ")
    print(prefix + connector + _label(store, node))
    child_prefix = prefix + ("   " if is_last else "│  ")
    for i, c in enumerate(children):
        _print_tree(store, c, child_prefix, i == len(children) - 1, skip_aux)


def cmd_tree(args):
    store = tree.Store.load(Path(args.file))
    if args.node_id:
        try:
            root = store.get(uuid.UUID(args.node_id))
        except (ValueError, AttributeError):
            print(f"无效节点 ID：{args.node_id}", file=sys.stderr)
            return 2
        if root is None:
            print(f"节点不存在：{args.node_id}", file=sys.stderr)
            return 2
        _print_tree(store, root, "", True, args.skip_aux)
        return 0
    for i, r in enumerate(store.roots()):
        if i > 0:
            print()  # 根之间空一行
        _print_tree(store, r, "", True, args.skip_aux)
    return 0


def cmd_validate(args):
    store = tree.Store.load(Path(args.file))
    errs = validator.validate(store.nodes())
    if not errs:
        print("校验通过：0 错误")
        return 0
    for e in errs:
        print(f"{e.code} <{e.node_id[:8]}…> {e.message}")
    print(f"共 {len(errs)} 个错误")
    return 1


# ---------------------------------------------------------------------------
# 入口
# ---------------------------------------------------------------------------

def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="xr", description="息壤 CLI（MVP：info / tree / validate）"
    )
    sub = parser.add_subparsers(dest="cmd", required=True)

    p = sub.add_parser("open", help="载入并打印摘要（等价 info）")
    p.add_argument("file")
    p.set_defaults(func=cmd_info)

    p = sub.add_parser("info", help="文件摘要")
    p.add_argument("file")
    p.set_defaults(func=cmd_info)

    p = sub.add_parser("tree", help="打印缩进树")
    p.add_argument("file")
    p.add_argument("--node", dest="node_id", help="只打印该节点为根的子树（UUID）")
    p.add_argument("--skip-aux", action="store_true", help="隐藏辅助节点（@ 前缀）")
    p.set_defaults(func=cmd_tree)

    p = sub.add_parser("validate", help="校验（E/R）")
    p.add_argument("file")
    p.set_defaults(func=cmd_validate)

    return parser


def main(argv=None):
    parser = build_parser()
    args = parser.parse_args(argv)
    try:
        return args.func(args)
    except (ValueError, FileNotFoundError, OSError) as e:
        print(f"错误：{e}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
