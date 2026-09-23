"""头文本多处副本必须逐字一致。

同一段英文自描述头文本存在于 README.md / README.en.md / tools/tree.py / rust/core/src/header.txt，
又被 scripts/gen_native_spec.py 抽进 4 个 `.xirang`。这里加一条守护测试：改一处就会红，
避免它静默分叉（它可是「大模型零提示读懂」的门面）。
"""

from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def _header_from_readme(filename: str, section: str) -> str:
    text = (ROOT / filename).read_text(encoding="utf-8")
    i = text.index(section)
    j = text.index("```", i) + 3
    k = text.index("```", j)
    return text[j:k].strip()


def test_header_copies_identical():
    from tools.tree import HEADER

    rust = (ROOT / "rust" / "core" / "src" / "header.txt").read_text(encoding="utf-8").strip()
    zh = _header_from_readme("README.md", "## 4. 头文本")
    en = _header_from_readme("README.en.md", "## 4. Header text")
    py = HEADER.strip()

    assert zh == en, "README.md 与 README.en.md 的头文本不一致"
    assert py == zh, "tools/tree.py 的 HEADER 与 README 不一致"
    assert rust == zh, "rust/core/src/header.txt 与 README 不一致"
