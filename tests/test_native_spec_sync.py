"""自举 `.xirang` 与 Markdown 的「漂移守护」。

自举文件的内容是**手写**在 `scripts/gen_native_spec.py` 里的（只有头文本是从 README 现抽），
所以 Markdown 改了不会自动同步。这里列一组「两边都必须出现」的关键串：
只要有人改了 Markdown 却忘了同步生成器 / 重新生成，测试就会红。
"""

from pathlib import Path

from tools.tree import Store

ROOT = Path(__file__).resolve().parent.parent


def _native_text(rel: str) -> str:
    s = Store.load(ROOT / rel)
    out = []
    for n in s.nodes():
        if n.name:
            out.append(n.name)
        tag, data = n.value
        if tag in (1, 2, 3, 4):  # 整数 / 浮点 / 布尔 / 文本
            out.append(str(data))
    return "\n".join(out)


# (Markdown 源, 自举文件, 必须出现的关键串)
PAIRS = [
    ("README.md", "spec/息壤文档.xirang", ["协议", "CLI", "MCP", "Skill", "rust/core", "rust/cli", "app", "255"]),
    ("spec/模板.md", "spec/模板.xirang", ["注释式", "@实例", "@模板", "格式转换"]),
    ("spec/版本规范.md", "spec/版本规范.xirang", ["shard-v1", "catalog-v1", "xirang-core", "xirang-app", "XRCAT", "XRIDX"]),
    ("spec/协议.md", "spec/协议.xirang", ["shard-v1", "catalog-v1", "XRIDX", "分片清单", "磁盘二分"]),
    ("errors/错误列表.md", "errors/错误列表.xirang", ["F009", "F010", "F011", "W001", "W008", "预留"]),
]


def test_native_specs_not_drifted():
    missing = []
    for md, xr, marks in PAIRS:
        text = _native_text(xr)
        for m in marks:
            if m not in text:
                missing.append(f"{xr} 里缺 {m!r}（应见 {md}）")
    assert not missing, "自举文件与 Markdown 漂移了：\n  " + "\n  ".join(missing)
