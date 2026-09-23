"""息壤参考实现（依赖内核 v1.0）。

- codec：节点 ↔ 字节编解码（内核）。
- validator：结构校验（节点层错误码 E/R）。
- tree：树处理层（遍历 / 寻址 / 落盘 / 文件头）。
- convert：格式转换（JSON / XML / YAML / Markdown）。
"""

__version__ = "0.1"
KERNEL_VERSION = "1.0"
