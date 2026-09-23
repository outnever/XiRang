#!/usr/bin/env python3
"""原生自举生成器：把息壤规范用息壤节点语言原生地表达（不是 Markdown 块 dump）。

生成（默认全部；可用 --only 选）：
- spec/息壤文档.xirang   内核规范（README）
- spec/模板.xirang        模板层规范
- spec/版本规范.xirang    版本规范
- spec/协议.xirang        协议登记（shard-v1 / catalog-v1 / XRIDX）
- errors/错误列表.xirang  错误列表

树结构用 Python 嵌套 dict 描述：
  {"name": 节点名, "value": str|int|float|bool, "children": [...]}
  value 缺省 = 空类型（纯容器）。根节点挂 @note/@source/@protocol/@history。

引用（引用边，构成图结构）用两个特殊键，生成后统一回填：
  {"name": 目标名, "label": "唯一标签"}      ← 目标节点，可被引用
  {"name": "...", "ref": "唯一标签"}         ← 该节点值 = 引用到「标签」对应的节点

用法：
  python scripts/gen_native_spec.py               # 生成全部 5 个文件
  python scripts/gen_native_spec.py --only template  # 只生成模板
"""
import argparse
import sys
import uuid
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))
from tools import codec
from tools.codec import Node, EMPTY, INT, FLOAT, BOOL, TEXT, REFERENCE, BLOB

REPO = "https://github.com/outnever/XiRang"

MIT_TEXT = (ROOT / "LICENSE").read_text(encoding="utf-8")


def extract_header() -> str:
    readme = (ROOT / "README.md").read_text(encoding="utf-8")
    i = readme.index("## 4. 头文本")
    j = readme.index("```", i) + 3
    k = readme.index("```", j)
    return readme[j:k].strip()


def to_value(v):
    """Python 值 → 息壤 (tag, data)。"""
    if v is None:
        return (EMPTY, None)
    if isinstance(v, bool):
        return (BOOL, v)
    if isinstance(v, int):
        return (INT, v)
    if isinstance(v, float):
        return (FLOAT, v)
    return (TEXT, str(v))


def build(nodes, refs, labels, parent, spec):
    n = Node(id=uuid.uuid4(), parent=parent.id if parent else None,
             name=spec["name"], value=(EMPTY, None))
    if "ref" in spec:
        refs.append((n, spec["ref"]))
    elif "value" in spec:
        n.value = to_value(spec["value"])
    nodes.append(n)
    if "label" in spec:
        labels[spec["label"]] = n.id
    for c in spec.get("children", []):
        build(nodes, refs, labels, n, c)
    return n


def resolve_refs(refs, labels):
    for n, target in refs:
        tid = labels.get(target)
        if tid is None:
            raise ValueError(f"引用目标不存在：{target}")
        n.value = (REFERENCE, tid)


def add_aux(nodes, parent, name, text):
    nodes.append(Node(id=uuid.uuid4(), parent=parent.id, name=name, value=(TEXT, text)))


def add_history(nodes, parent, entries, now):
    h = Node(id=uuid.uuid4(), parent=parent.id, name="@history", value=(EMPTY, None))
    nodes.append(h)
    for name, note in entries:
        e = Node(id=uuid.uuid4(), parent=h.id, name=name, value=(TEXT, note))
        nodes.append(e)
        nodes.append(Node(id=uuid.uuid4(), parent=e.id, name="@created", value=(TEXT, now)))


def write_file(path, root_name, root_note, root_source, protocol, history, tree):
    nodes = []
    refs = []
    labels = {}
    now = datetime.now(timezone.utc).isoformat()
    root = Node(id=uuid.uuid4(), parent=None, name=root_name, value=(EMPTY, None))
    nodes.append(root)
    add_aux(nodes, root, "@note", root_note)
    add_aux(nodes, root, "@source", root_source)
    add_aux(nodes, root, "@protocol", protocol)
    add_history(nodes, root, history, now)
    for child in tree:
        build(nodes, refs, labels, root, child)
    resolve_refs(refs, labels)

    header = extract_header().encode("utf-8")
    node_bytes = b"".join(codec.encode_node(n) for n in nodes)
    payload = b"XRNG" + bytes([1]) + len(header).to_bytes(4, "big") + header + node_bytes
    path.write_bytes(payload)
    print(f"{path.name}: {len(nodes)} 节点, {len(payload)} 字节")


# ============================================================================
# 内核规范（README）
# ============================================================================

KERNEL_TREE = [
    {"name": "说明", "value": "一种极小的节点语言格式：文件里只有节点，节点只有编号、父节点、名字、值四样。"},
    {"name": "版本", "value": "1.0"},
    {"name": "github", "value": REPO},
    {"name": "协议", "value": "MIT License", "children": [
        {"name": "内容", "value": MIT_TEXT},
    ]},
    {"name": "为什么选息壤", "children": [
        {"name": "优势", "value": "格式几乎不做规定：不绑列、不绑嵌套、不绑标签，怎么组织全交给用的人；约定少，所以面向上千种用途都不用改格式本身。"},
        {"name": "用途", "value": "词库、术语表、知识条目、分类体系、文档结构、配置、实验记录等「想存一棵会长的树、又不想被格式框住」的地方；同一份数据可当树遍历、可当图（节点靠引用相连）、还可导出成 JSON/XML/YAML/Markdown。"},
        {"name": "与其他格式比较", "value": "比 JSON/XML/YAML：那些是文本格式，但没有类型语义、也没有跨文件的节点身份；息壤是二进制、带类型标记与全局唯一编号，天生支持跨文件引用与大树。比数据库：数据库擅长查询管理，但数据要先「进库」；息壤就是文件本身，随仓库走、直接分发。比自造二进制格式：息壤多了自描述文件头与节点类型，不必翻代码就能解析。"},
        {"name": "二进制是否难读", "value": "不能像文本格式那样用记事本直接读、也不方便手敲；但可由大模型当助手、或调用 CLI / 可视化工具来读和写，这些短板基本能绕开。"},
    ]},
    {"name": "大模型使用", "value": "息壤文件自带纯英文自描述头，大模型零提示即可读懂；且对这种格式相当友好。", "children": [
        {"name": "推荐流程", "value": "1) 把 .xirang（或十六进制 dump）交给大模型；2) 读文件头，明白结构；3) 调用官方工具（Store.load）转成可读树再读；4) 仅需徒手逐字节才按头文本解码。"},
        {"name": "模型友好", "value": "9B 以上的大模型不需要任何解析工具，仅凭文件头就能完整解析出文件里的所有节点；配合 CLI 还能便利地检索、新增、修订节点。"},
    ]},
    {"name": "程序使用", "value": "用官方工具读成节点树。", "children": [
        {"name": "示例", "value": "from tools.tree import Store\nstore = Store.load(\"spec/息壤文档.xirang\")"},
    ]},
    {"name": "文件格式", "children": [
        {"name": "后缀", "value": ".xirang"},
        {"name": "结构", "value": "文件头 + 节点序列，所有多字节整数大端序"},
        {"name": "文件头", "children": [
            {"name": "magic", "value": "固定 ASCII XRNG（4 字节）", "children": [
                {"name": "例", "value": "58 52 4e 47 = XRNG"},
            ]},
            {"name": "格式版本", "value": "当前 = 1（1 字节），决定后续字段布局", "children": [
                {"name": "例", "value": "01 = 版本 1"},
            ]},
            {"name": "头文本长度", "value": "头文本字节数（4 字节大端），可为 0", "children": [
                {"name": "例", "value": "00 00 00 2a = 42 字节"},
            ]},
            {"name": "头文本", "value": "UTF-8 纯文本"},
        ]},
        {"name": "完整例子", "value": "无头文本的最小文件头", "children": [
            {"name": "字节", "value": "58 52 4e 47 01 00 00 00 00   ← magic + 版本 1 + 头长 0"},
        ]},
    ]},
    {"name": "节点", "children": [
        {"name": "顺序", "value": "节点编号 → 父节点 → 节点名 → 节点值"},
        {"name": "字段", "children": [
            {"name": "节点编号", "children": [
                {"name": "说明", "value": "节点身份 UUID，全库唯一、必填、不可改写。"},
                {"name": "字节长度", "value": 16},
                {"name": "例", "value": "550e8400-e29b-41d4-a716-446655440000"},
            ]},
            {"name": "父节点", "children": [
                {"name": "说明", "value": "父节点的节点编号；全 0 = 根节点（无父）。"},
                {"name": "字节长度", "value": 16},
                {"name": "例", "value": "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 = 根节点"},
            ]},
            {"name": "节点名", "children": [
                {"name": "说明", "value": "名字，可空；上限 255 字节（超限报 F009）。"},
                {"name": "编码", "value": "1 字节长度 + N 字节 UTF-8"},
                {"name": "例", "value": "03 e7 81 af = 灯（1 字节长度 3 + 3 字节 UTF-8）"},
            ]},
            {"name": "节点值", "children": [
                {"name": "编码", "value": "1 字节类型标记 + 长度字段 + 值内容"},
                {"name": "空", "value": "无内容，纯容器节点（标记 0）", "children": [
                    {"name": "例", "value": "00"},
                ]},
                {"name": "整数", "value": "8 字节有符号 int64（标记 1）", "children": [
                    {"name": "例", "value": "01 00 00 00 00 00 00 07 fe = 2046"},
                ]},
                {"name": "浮点数", "value": "8 字节 IEEE 754 double（标记 2）", "children": [
                    {"name": "例", "value": "02 3f f8 00 00 00 00 00 00 = 1.5"},
                ]},
                {"name": "布尔", "value": "1 字节，0 假 / 1 真（标记 3）", "children": [
                    {"name": "例", "value": "03 01 = true"},
                ]},
                {"name": "文本", "value": "4 字节长度 + UTF-8（标记 4）", "children": [
                    {"name": "例", "value": "04 00 00 00 03 e7 81 af = 灯"},
                ]},
                {"name": "引用", "value": "16 字节 UUID，指向另一节点，构成关联边（标记 5）", "children": [
                    {"name": "例", "value": "05 55 0e 84 00 e2 9b 41 d4 a7 16 44 66 55 44 00 00 = 引用 → 该节点"},
                ]},
                {"name": "二进制块", "value": "8 字节长度 + 原始字节（标记 6）", "children": [
                    {"name": "例", "value": "06 00 00 00 00 00 00 00 03 ff ff ff = 3 字节原始内容"},
                ]},
            ]},
        ]},
        {"name": "完整例子", "value": "根节点，名「灯」，值为空", "children": [
            {"name": "字节", "value": "55 0e 84 00 e2 9b 41 d4 a7 16 44 66 55 44 00 01   ← 节点编号\n00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00   ← 父节点（根）\n03 e7 81 af   ← 节点名 灯\n00   ← 节点值（空）"},
        ]},
    ]},
    {"name": "头文本", "children": [
        {"name": "说明", "value": "写入文件头的纯英文自描述文本，让不懂息壤的读取方（含大模型）零提示读懂。"},
        {"name": "原则", "value": "只描述如何解析，不做判定。"},
        {"name": "内容", "value": extract_header()},
    ]},
    {"name": "文档", "children": [
        {"name": "模板", "value": "spec/模板.md（可选，仅供参考）：息壤节点怎么组织、辅助节点、格式、转换、案例。"},
        {"name": "协议", "value": "spec/协议.md：登记「树怎么组织、怎么存」的约定（如分片词库、本机目录）。"},
        {"name": "版本规范", "value": "spec/版本规范.md（维护者参考）。"},
        {"name": "错误列表", "value": "errors/错误列表.md（实现参考）。"},
        {"name": "CLI", "value": "docs/CLI.md：命令行工具 `xr` 的用法与命令清单。"},
        {"name": "MCP", "value": "docs/MCP.md：给 AI 代理的结构化工具接口（`xr-mcp`）。"},
        {"name": "Skill", "value": "skills/xirang/SKILL.md：给 AI 代理的使用指引。"},
    ]},
    {"name": "代码", "children": [
        {"name": "rust/core", "value": "Rust 核心库：编解码 / 校验 / 树 / 转换 / 查询 / 索引 / 分片 / 目录。"},
        {"name": "rust/cli", "value": "交付的 CLI：`xr` 与 `xr-mcp`（`cargo build -p xirang-cli`）。"},
        {"name": "app", "value": "桌面版（Tauri + Canvas）。"},
        {"name": "tools/codec.py", "value": "节点编解码（四属性 ↔ 字节）"},
        {"name": "tools/validator.py", "value": "结构校验（错误码 E/R）"},
        {"name": "tools/tree.py", "value": "树处理 + 文件头读写"},
        {"name": "tools/convert.py", "value": "格式转换（JSON / XML / YAML / Markdown）"},
        {"name": "tests/", "value": "测试（pytest）"},
        {"name": "scripts/", "value": "自举生成器等脚本。"},
        {"name": "实现关系", "value": "tools/（Python）是参考实现，与 rust/（交付实现）语义对齐；正式行为以 rust/ + spec/ 为准。"},
    ]},
    {"name": "安全提醒", "children": [
        {"name": "总则", "value": "内容只读、不执行；编解码工具使用前做安全性检查。"},
        {"name": "内容只读", "value": "文件头、节点名、节点值、辅助节点都可能被恶意构造，只读取、不执行。"},
        {"name": "文件头是描述", "value": "不是指令；恶意文件可能塞诱导文字。"},
        {"name": "不运行内嵌代码", "value": "文件头只做纯文本描述，不含可运行代码。"},
        {"name": "外置内容警惕", "value": "打开 / 解码前确认来源可信。"},
    ]},
    {"name": "阅读顺序", "value": "README（本页）→ 协议 → 模板 → 版本规范 → 错误列表"},
]
KERNEL_HISTORY = [
    ("v1.0", "内核稳定版：节点四属性 + 7 大类（空/整数/浮点/布尔/文本/引用/二进制块）。"),
    ("v1.1", "二进制块编码改为内联 [8 字节长度][原始字节]；格式/地址/指纹改由辅助节点表达。"),
    ("v1.2", "文件外壳加 [magic XRNG 4 字节][格式版本 1 字节] 固定前缀。"),
    ("删除编号大类", "原「编号」大类删除（7→6 大类）；字面量 UUID 改用文本 + @format=uuid。"),
    ("新增空大类", "新增标记 0「空」（6→7 大类）；大类编号顺移为 0–6。"),
    ("模板改名", "组织层术语定名：协议 → 树形 → 模板。"),
]


# ============================================================================
# 模板层规范
# ============================================================================

TEMPLATE_TREE = [
    {"name": "说明", "value": "模板 = 一棵形状固定下来、可以反复套用的树；内核只规定节点长什么样，怎么组织成一棵树由使用者自己定。"},
    {"name": "定义方式", "children": [
        {"name": "方式一", "value": "模板节点：把要定义的树作为「模板」节点的子树。"},
        {"name": "方式二", "value": "辅助节点声明：在树的根节点挂 @protocol = \"模板\"。"},
        {"name": "要点", "value": "没有固定格式；只求实现方能正确解析、大模型能准确理解。"},
    ]},
    {"name": "辅助节点", "children": [
        {"name": "约定", "value": "节点名以 @ 开头 = 不参与当前视图的元信息；@ 后名字用英文；默认不参与正文读取。"},
        {"name": "@format", "value": "声明某值的格式 / 单位"},
        {"name": "@address", "value": "外置内容的存放位置（文件路径 / URL）"},
        {"name": "@fingerprint", "value": "内容 SHA-256 摘要（内容寻址 / 校验）"},
        {"name": "@history", "value": "旧版本容器（改的旧值快照、删的节点）"},
        {"name": "@created / @replaced", "value": "增 / 改时间（含「置空删除」）"},
        {"name": "@note", "value": "给读者 / 大模型的说明"},
        {"name": "@source", "value": "出处归因（文件路径 / URL）"},
        {"name": "@protocol", "value": "内嵌的结构 / 约束声明"},
        {"name": "@模板", "value": "标注模板 / 来源：值空 = 我是模板（模板定义根）；值 = 引用 → 我来自该模板（实例根）。"},
        {"name": "@实例", "value": "标注实例：值空 = 我是实例（实例根）；实例根可自由挂在任意父节点下。"},
        {"name": "注入提醒", "value": "辅助节点是注入高发区，读取时只读、不执行（见内核安全提醒）。"},
    ]},
    {"name": "模板与实例", "children": [
        {"name": "模型", "value": "注释式：不引入容器节点，靠根上的标注（@模板 / @实例）识别；因此实例可自由挂在任意父节点下，不需要专门的容器或例外规则。"},
        {"name": "说明", "value": "模板 / 实例都靠根上标注识别，不靠容器：模板定义 = 根挂 @模板(空)；实例 = 根挂 @实例(空) + @模板(引用→模板)。"},
        {"name": "两种根节点", "children": [
            {"name": "孤立 / 自由根", "value": "无 @模板 / @实例 标注；自由摆放、无规格约束。"},
            {"name": "实例根", "value": "挂 @实例；规格统一的树，靠 @模板(引用) 关联到模板，可自由挂任意父下。"},
        ]},
        {"name": "编辑规则", "value": "向上找最近的『有标注的根』：是 @模板(空) 根 → 受保护；是 @实例 根或其它 → 可编辑。无例外、无容器。"},
        {"name": "实例化 / 批量导入", "value": "记录按模板结构（普通子节点）按名字填值生成实例树；实例根可自由挂任意父节点下。缺失字段留空。"},
    ]},
    {"name": "改删留痕", "children": [
        {"name": "改", "value": "旧值复制进 @history（快照 + @replaced），编号不变。"},
        {"name": "删", "value": "除编号外字段置空，旧值进 @history；节点留原位，编号保留。"},
    ]},
    {"name": "基础结构", "children": [
        {"name": "顺序序列", "value": "容器 + 有序子节点，按位置编号（从 0 起）；数组/元组/队列/栈 同一形态，用 @protocol 区分语义。", "children": [
            {"name": "例子", "children": [
                {"name": "例句", "children": [
                    {"name": "@note", "value": "同一形态，用 @protocol 区分：数组=同质可增、元组=异质固定、队列=先进先出、栈=后进先出"},
                    {"name": "@protocol", "value": "数组：按位置编号、有序（从 0 起）"},
                    {"name": "0", "value": "床头柜上有一盏灯。"},
                    {"name": "1", "value": "路灯在傍晚自动亮起。"},
                    {"name": "2", "value": "台灯放在书桌上。"},
                ]},
            ]},
        ]},
        {"name": "键值容器", "value": "容器 + 有名子节点（名=键、值=值、同父键唯一）；map / 记录 同一形态，用 @protocol 区分。", "children": [
            {"name": "例子", "children": [
                {"name": "员工信息", "children": [
                    {"name": "@note", "value": "同一形态，用 @protocol 区分：map=通用键值、记录=结构化字段"},
                    {"name": "名称", "value": "张三"},
                    {"name": "年龄", "value": 30},
                    {"name": "活跃", "value": True},
                ]},
            ]},
        ]},
        {"name": "集合", "value": "容器 + 子节点（元素唯一、无序，不按位置）。", "children": [
            {"name": "例子", "children": [
                {"name": "水果", "children": [
                    {"name": "@protocol", "value": "集合：元素唯一、无序"},
                    {"name": "苹果"},
                    {"name": "香蕉"},
                    {"name": "橙子"},
                ]},
            ]},
        ]},
        {"name": "枚举", "value": "容器 + 值域子节点 + 引用指向当前值；开/闭由 @protocol 声明。", "children": [
            {"name": "例子", "children": [
                {"name": "词性", "children": [
                    {"name": "@protocol", "value": "闭枚：值域固定，只能取下面之一"},
                    {"name": "@note", "value": "开枚同理：@protocol 改为「开枚：值域可扩展」即可"},
                    {"name": "当前值", "ref": "sx:名词"},
                    {"name": "名词", "label": "sx:名词"},
                    {"name": "动词"},
                    {"name": "形容词"},
                ]},
            ]},
        ]},
        {"name": "时间", "value": "整数（Unix 秒）或文本（ISO 字符串）。", "children": [
            {"name": "例子", "children": [
                {"name": "事件", "children": [
                    {"name": "发生时刻", "value": 1704067200, "children": [
                        {"name": "@format", "value": "unix timestamp"},
                    ]},
                    {"name": "持续时长", "value": 3600, "children": [
                        {"name": "@format", "value": "seconds"},
                    ]},
                ]},
            ]},
        ]},
        {"name": "定点数", "value": "整数 + @protocol 声明小数位。", "children": [
            {"name": "例子", "children": [
                {"name": "价格", "value": 12345, "children": [
                    {"name": "@protocol", "value": "fixed point: 2（→ 123.45）"},
                ]},
            ]},
        ]},
        {"name": "复数", "value": "实部 / 虚部两个子节点。", "children": [
            {"name": "例子", "children": [
                {"name": "复数", "children": [
                    {"name": "@protocol", "value": "复数：实部 + 虚部"},
                    {"name": "实部", "value": 3.0},
                    {"name": "虚部", "value": 4.0},
                ]},
            ]},
        ]},
        {"name": "矩阵 / 张量", "value": "数组嵌套 + @protocol 声明形状。", "children": [
            {"name": "例子", "children": [
                {"name": "矩阵", "children": [
                    {"name": "@protocol", "value": "矩阵 2×2：嵌套数组 + 形状"},
                    {"name": "行1", "children": [
                        {"name": "", "value": 1},
                        {"name": "", "value": 2},
                    ]},
                    {"name": "行2", "children": [
                        {"name": "", "value": 3},
                        {"name": "", "value": 4},
                    ]},
                ]},
            ]},
        ]},
        {"name": "正则 / 富文本", "value": "文本 / 二进制块 + @format。", "children": [
            {"name": "例子", "children": [
                {"name": "富文本", "children": [
                    {"name": "内容", "value": "<p>你好</p>"},
                    {"name": "@format", "value": "html"},
                ]},
            ]},
        ]},
        {"name": "跨系统引用", "value": "文本 + @format = \"uuid\"（外部 ID）。", "children": [
            {"name": "例子", "children": [
                {"name": "外部词条", "children": [
                    {"name": "值", "value": "550e8400-e29b-41d4-a716-446655440000"},
                    {"name": "@format", "value": "uuid"},
                ]},
            ]},
        ]},
    ]},
{"name": "图", "children": [
        {"name": "顶点", "value": "图论里的「顶点」对应一棵子树，不是一个节点。"},
        {"name": "边", "value": "引用（顶点 A 的引用 → 顶点 B 的子树根）。"},
        {"name": "环", "value": "引用边可成环；E006 只禁父边环，R001 只查引用目标存在。"},
        {"name": "状态机", "value": "顶点带属性 + 转移成环。", "children": [
            {"name": "例子", "children": [
                {"name": "状态：空闲", "label": "gx:空闲", "children": [
                    {"name": "说明", "value": "等待任务"},
                    {"name": "转移", "children": [
                        {"name": "收到任务", "ref": "gx:处理中"},
                        {"name": "超时", "ref": "gx:空闲"},
                    ]},
                ]},
                {"name": "状态：处理中", "label": "gx:处理中", "children": [
                    {"name": "说明", "value": "执行中"},
                    {"name": "转移", "children": [
                        {"name": "完成", "ref": "gx:空闲"},
                        {"name": "失败", "ref": "gx:重试"},
                    ]},
                ]},
                {"name": "状态：重试", "label": "gx:重试", "children": [
                    {"name": "说明", "value": "失败后重试"},
                    {"name": "转移", "children": [
                        {"name": "成功", "ref": "gx:处理中"},
                    ]},
                ]},
            ]},
        ]},
        {"name": "依赖图", "value": "多对多 + 共享依赖。", "children": [
            {"name": "例子", "children": [
                {"name": "包A", "label": "gx:A", "children": [
                    {"name": "版本", "value": "1.0"},
                    {"name": "依赖", "children": [
                        {"name": "", "ref": "gx:B"},
                        {"name": "", "ref": "gx:C"},
                    ]},
                ]},
                {"name": "包B", "label": "gx:B", "children": [
                    {"name": "版本", "value": "2.0"},
                    {"name": "依赖", "children": [
                        {"name": "", "ref": "gx:C"},
                    ]},
                ]},
                {"name": "包C", "label": "gx:C", "children": [
                    {"name": "版本", "value": "1.5"},
                ]},
            ]},
        ]},
        {"name": "流程图", "value": "循环回边。", "children": [
            {"name": "例子", "children": [
                {"name": "开始", "label": "gx:开始", "children": [
                    {"name": "下一步", "ref": "gx:判断"},
                ]},
                {"name": "判断", "label": "gx:判断", "children": [
                    {"name": "分支", "children": [
                        {"name": "是", "ref": "gx:处理"},
                        {"name": "否", "ref": "gx:结束"},
                    ]},
                ]},
                {"name": "处理", "label": "gx:处理", "children": [
                    {"name": "完成", "ref": "gx:判断"},
                ]},
                {"name": "结束", "label": "gx:结束"},
            ]},
        ]},
    ]},
    {"name": "应用场景", "children": [
        {"name": "写作", "value": "事件 / 人物关系。", "children": [
            {"name": "例子", "children": [
                {"name": "张三", "label": "ex:张三", "value": "人物", "children": [
                    {"name": "名字", "value": "张三"},
                    {"name": "住所", "ref": "ex:北京"},
                ]},
                {"name": "北京", "label": "ex:北京", "value": "地点", "children": [
                    {"name": "名字", "value": "北京"},
                ]},
                {"name": "事件-2024-001", "children": [
                    {"name": "主体", "ref": "ex:张三"},
                    {"name": "地点", "ref": "ex:北京"},
                    {"name": "时间", "value": 1715328000, "children": [
                        {"name": "@format", "value": "unix timestamp"},
                    ]},
                    {"name": "前因", "ref": "ex:事件-2023-999"},
                ]},
                {"name": "事件-2023-999", "label": "ex:事件-2023-999", "children": [
                    {"name": "主体", "ref": "ex:张三"},
                    {"name": "地点", "ref": "ex:北京"},
                ]},
            ]},
        ]},
        {"name": "记忆", "value": "陈述性记忆 + @source 出处归因。", "children": [
            {"name": "例子", "children": [
                {"name": "事件-2024-001", "children": [
                    {"name": "主体", "ref": "ex:张三"},
                    {"name": "时间", "value": 1715328000, "children": [
                        {"name": "@format", "value": "unix timestamp"},
                    ]},
                    {"name": "描述", "value": "张三去了北京"},
                    {"name": "@source", "value": "file:///data/raw/dlg42.txt"},
                ]},
            ]},
        ]},
        {"name": "词库", "value": "词条（词形 / 词频 / 词义 / 词性）。", "children": [
            {"name": "例子", "children": [
                {"name": "灯", "children": [
                    {"name": "词形", "value": "灯"},
                    {"name": "词频", "value": 2046},
                    {"name": "词义", "children": [
                        {"name": "01", "children": [
                            {"name": "释义", "value": "照明或做其他用途的发光器具"},
                            {"name": "词性", "children": [
                                {"name": "名词", "children": [
                                    {"name": "类属层级", "value": "类"},
                                    {"name": "父类", "ref": "ex:光源"},
                                ]},
                            ]},
                        ]},
                    ]},
                ]},
                {"name": "光源", "label": "ex:光源", "children": [
                    {"name": "释义", "value": "能发光的物体"},
                ]},
            ]},
        ]},
    ]},
    {"name": "自举与其他案例", "children": [
        {"name": "说明", "value": "模板层自举与复用示例（见 spec/模板.md 八）。"},
        {"name": "息壤文档用息壤存", "children": [
            {"name": "说明", "value": "息壤自己的文档（README、模板、版本规范、错误列表）也用息壤格式存储，可由解析程序或大模型直接读。"},
        ]},
        {"name": "程序词库", "value": "带版本维度。", "children": [
            {"name": "例子", "children": [
                {"name": "String（API，Java）", "children": [
                    {"name": "方法", "children": [
                        {"name": "length()", "children": [
                            {"name": "签名", "value": "int length()"},
                            {"name": "@history", "children": [
                                {"name": "v1", "value": "Java 8：返回 UTF-16 单元数"},
                                {"name": "v2", "value": "Java 18：按码点计数"},
                            ]},
                        ]},
                    ]},
                ]},
            ]},
        ]},
        {"name": "生物分类", "value": "层级 + 复用。", "children": [
            {"name": "例子", "children": [
                {"name": "动物界", "children": [
                    {"name": "脊索动物门", "children": [
                        {"name": "哺乳纲", "children": [
                            {"name": "食肉目", "children": [
                                {"name": "猫科", "children": [
                                    {"name": "家猫", "value": "Felis catus"},
                                ]},
                            ]},
                        ]},
                    ]},
                ]},
            ]},
        ]},
    ]},
    {"name": "格式转换", "children": [
        {"name": "无损", "value": "JSON / XML / YAML 无损往返。"},
        {"name": "有损", "value": "Markdown 有损、只导出。"},
        {"name": "空对象", "value": "{} → 空类型节点；空字符串 \"\" → 空文本。"},
        {"name": "例：JSON → 节点树", "children": [
            {"name": "光源", "label": "fx:光源", "children": [
                {"name": "释义", "value": "能发光的物体"},
            ]},
            {"name": "灯", "children": [
                {"name": "词形", "value": "灯"},
                {"name": "词义", "children": [
                    {"name": "释义", "value": "照明器具"},
                ]},
                {"name": "备注"},
                {"name": "父类", "ref": "fx:光源"},
            ]},
        ]},
        {"name": "例：节点树 → JSON", "children": [
            {"name": "说明", "value": "节点树导出为 JSON：无损保留编号/值类型标记；有损丢失编号/类型标记、引用退化为节点名。"},
            {"name": "无损结果", "value": "{\"format\":\"xirang\",\"kernel\":\"1.0\",\"nodes\":[{\"id\":\"…0000\",\"parent\":null,\"name\":\"光源\",\"value\":{\"type\":\"text\",\"value\":\"能发光的物体\"}},{\"id\":\"…0001\",\"parent\":null,\"name\":\"灯\",\"value\":{\"type\":\"empty\",\"value\":null}},{\"id\":\"…0002\",\"parent\":\"…0001\",\"name\":\"词形\",\"value\":{\"type\":\"text\",\"value\":\"灯\"}},{\"id\":\"…0003\",\"parent\":\"…0001\",\"name\":\"词义\",\"value\":{\"type\":\"empty\",\"value\":null}},{\"id\":\"…0004\",\"parent\":\"…0003\",\"name\":\"释义\",\"value\":{\"type\":\"text\",\"value\":\"照明器具\"}},{\"id\":\"…0005\",\"parent\":\"…0001\",\"name\":\"备注\",\"value\":{\"type\":\"empty\",\"value\":null}},{\"id\":\"…0006\",\"parent\":\"…0001\",\"name\":\"父类\",\"value\":{\"type\":\"reference\",\"value\":\"…0000\"}}]}", "children": [{"name": "@format", "value": "json"}]},
            {"name": "有损结果", "value": "{\"光源\":{\"释义\":\"能发光的物体\"},\"灯\":{\"词形\":\"灯\",\"词义\":{\"释义\":\"照明器具\"},\"备注\":{},\"父类\":\"光源\"}}", "children": [{"name": "@format", "value": "json"}]},
        ]},
    ]},
    {"name": "二进制块", "children": [
        {"name": "@format", "value": "声明格式 / 单位（全小写、用扩展名）。"},
        {"name": "@fingerprint", "value": "SHA-256 摘要，内容寻址 / 校验。"},
    ]},
]
TEMPLATE_HISTORY = [
    ("v1.0", "模板层定稿：怎么定义模板、辅助节点、基础结构、图、格式转换。"),
    ("改名", "组织层术语：协议 → 树形 → 模板。"),
]


# ============================================================================
# 版本规范
# ============================================================================

VERSION_TREE = [
    {"name": "说明", "value": "每一块各管各的版本号，只在标题里写清「我是基于哪个内核版本做的」；普通使用者不必关心，看文件头里的格式版本即可。"},
    {"name": "原则", "value": "解耦 + 依赖声明：只有内核改动才升内核版本；外围各自独立版本并声明依赖的内核版本。"},
    {"name": "版本分层", "children": [
        {"name": "内核", "value": "README（节点 / 节点值），vMAJOR.MINOR。", "children": [
            {"name": "升版规则", "value": "只有四属性 / 值编码 / 文件外壳改动才升；MAJOR=破坏性、MINOR=兼容新增。"},
        ]},
        {"name": "文件格式", "value": "文件内「格式版本字节」，单调递增。", "children": [
            {"name": "升版规则", "value": "跟随内核 MAJOR。"},
        ]},
        {"name": "模板 / 约定", "value": "spec/模板.md，各自 vX.Y。", "children": [
            {"name": "升版规则", "value": "各自独立升版。"},
        ]},
        {"name": "协议", "value": "spec/协议.md，各自 vX.Y。", "children": [
            {"name": "升版规则", "value": "各自独立升版。"},
        ]},
        {"name": "实现", "value": "tools/，各自 vX.Y。", "children": [
            {"name": "升版规则", "value": "各自独立升版。"},
        ]},
        {"name": "错误码", "value": "errors/错误列表.md，各自 vX.Y。", "children": [
            {"name": "升版规则", "value": "各自独立升版。"},
        ]},
    ]},
    {"name": "升版规则", "children": [
        {"name": "内核 MAJOR", "value": "破坏性（旧解析器读不了）。"},
        {"name": "内核 MINOR", "value": "兼容新增（如新增类型标记）。"},
    ]},
    {"name": "依赖声明", "value": "每份外围文档 / 实现在标题处声明所依赖的内核版本。"},
    {"name": "不做的", "children": [
        {"name": "不把依赖拼进版本号", "value": "不用 1.0-0.5-0.3 这种链式。"},
        {"name": "不因外围改动升内核", "value": ""},
    ]},
    {"name": "兼容矩阵", "children": [
        {"name": "内核", "value": "v1.0（依赖内核：—）"},
        {"name": "模板", "value": "v1.0（依赖内核 v1.0）"},
        {"name": "协议 shard-v1", "value": "v1.0（依赖内核 v1.0）"},
        {"name": "协议 catalog-v1", "value": "v1.0（依赖内核 v1.0）"},
        {"name": "实现 tools（Python 参考）", "value": "v0.1（依赖内核 v1.0）"},
        {"name": "实现 xirang-core / xirang-cli（Rust 交付）", "value": "0.1.0（依赖内核 v1.0）"},
        {"name": "桌面 xirang-app", "value": "0.1.0（依赖内核 v1.0）"},
        {"name": "盘上格式", "value": "文件格式 1 · XRIDX 2 · XRCAT 2"},
    ]},
]

VERSION_HISTORY = [
    ("v1.0", "版本规范定稿：解耦 + 依赖声明。"),
]


# ============================================================================
# 错误列表
# ============================================================================

def _err(code, name, desc, note=None):
    node = {"name": f"{code} {name}", "value": desc}
    if note:
        node["children"] = [{"name": "备注", "value": note}]
    return node

ERROR_CODES = [
    _err("E001", "编号缺失", "节点编号为 nil（16 字节全 0）。"),
    _err("E002", "编号冲突", "两个节点编号相同。"),
    _err("E003", "编号格式非法", "编号不是合法 UUID。"),
    _err("E005", "父节点自指", "父节点 = 节点自身。"),
    _err("E006", "父节点成环", "沿父节点向上回到自己。"),
    _err("E008", "类型标记非法", "类型标记不在 0-6。"),
    _err("E009", "值内容非法", "值内容与类型标记不符。"),
    _err("E011", "父节点断裂", "父节点编号在库中不存在。"),
    _err("R001", "引用断裂", "引用指向的编号在库中不存在。"),
    _err("F001", "魔数非法", "文件最前 4 字节不是 XRNG。"),
    _err("F002", "格式版本不支持", "格式版本不是当前支持的版本。"),
    _err("F003", "头长非法", "头长字段为负或超出文件剩余长度。"),
    _err("F004", "文件截断", "节点流在节点中间结束。"),
    _err("F005", "索引魔数非法", "sidecar 索引最前 5 字节不是 XRIDX。"),
    _err("F006", "索引版本不支持", "sidecar 索引版本不是当前支持的版本。"),
    _err("F007", "索引损坏 / 截断", "sidecar 索引条目越界或字节不足。"),
    _err("F008", "索引与源文件不一致", "索引偏移读出的节点编号与目标不符。"),
    _err("F009", "节点名超长", "节点名超过 255 字节上限。"),
    _err("F010", "类型标记非法", "解码时节点值的类型标记不在 0-6。"),
    _err("F011", "文本非法 UTF-8", "解码时节点名 / 文本值不是合法 UTF-8。"),
    _err("C001", "类型名非法", "格式转换时 type 不是 7 种之一。"),
    _err("C002", "UUID 非法", "编号 / 父节点 / 引用值不是合法 UUID。"),
    _err("C003", "二进制块编码非法", "内联 blob 的 value 不是合法 Base64。"),
    _err("C004", "外置文件缺失", "blob 的 ref 指向的文件不存在。"),
    _err("C005", "节点字段缺失", "节点缺 id / name / value 之一。"),
    _err("W001", "分片清单缺失", "找不到 manifest.xirang 或缺 shard-v1 协议标记。"),
    _err("W002", "分片文件缺失", "清单登记的分片文件在目录中不存在。"),
    _err("W003", "分片重复", "多个分片声明了同一个分片根 UUID。", "预留：当前实现不主动抛出（重复由 xr collection list 显示）。"),
    _err("W004", "覆盖日志损坏", "分片的追加修订记录无法解析 / 折叠。", "预留：当前实现不主动抛出（损坏由 Store::load 报 F004）。"),
    _err("W005", "分片根判定器非法", "判定器不是 root / name / depth / marker。"),
    _err("W006", "目录损坏", "本机目录文件无法解析（缓存，可删除重建）。"),
    _err("W007", "目录冲突未解决", "同一编号出现在多个文件、且节点自身（名字 / 值）对不上。", "预留：当前实现不主动抛出（同编号不冲突；用 xr catalog check 查看自身不一致的编号）。"),
    _err("W008", "目录条目失效", "目录记录的文件路径已不存在 / 指纹不符。", "预留：当前实现不主动抛出（scan 覆盖 / forget 移除）。"),
]

ERROR_TREE = [
    {"name": "原则", "value": "错误码稳定：只追加、不重排、不复用。格式 X<三位>。"},
    {"name": "怎么读编号", "value": "首字母表示哪一层报出来的，字母后的三位数字是这一层里的第几个错误；看到 E005、W003 这样的编号，按编号查即可。"},
    {"name": "分层", "children": [
        {"name": "节点层", "value": "E 结构 / R 引用（校验器报）。"},
        {"name": "模板层", "value": "V 版本 / O 顺序（预留，暂未定义）。"},
        {"name": "实现层", "value": "F 文件（读取器报）/ C 转换（格式转换报）/ W 写入器（已生效）。"},
    ]},
    {"name": "错误码", "children": ERROR_CODES},
    {"name": "使用约定", "children": [
        {"name": "节点层", "value": "校验器输出 码 + 节点编号 + 描述，如 R001 <w-8f2c…> 引用目标不存在。"},
        {"name": "实现层", "value": "读取器 / 格式转换抛异常，信息含码，如 F001：魔数非法、C001：类型名非法。"},
        {"name": "外部格式错误", "value": "JSON / XML / YAML 解析失败由对应解析器报告，不另设息壤错误码。"},
        {"name": "只追加", "value": "错误码只在新增时追加，永不重排 / 复用，保证日志可长期追溯。"},
    ]},
]

ERROR_HISTORY = [
    ("v1.0", "错误列表定稿：节点层 E/R、实现层 F/C，模板层 V/O 预留。"),
]


# ============================================================================
# 协议（spec/协议.md）
# ============================================================================

PROTOCOL_TREE = [
    {"name": "说明", "value": "协议 = 约定「树怎么组织、怎么存」；内核只定义节点本身（四属性 + 7 大类）。同一份数据可以有不同的组织方式，协议可扩展、可替换。"},
    {"name": "怎么读", "value": "每条协议先给一句大白话「它解决什么问题」，再给具体规则；不要求读者懂技术。"},
    {"name": "已登记协议", "children": [
        {"name": "shard-v1 分片词库", "children": [
            {"name": "一句话", "value": "把一个很大的文件拆成「一个目录 + 很多小分片文件 + 一份清单」；改一个词只动一个小文件，不用重写整个大文件。"},
            {"name": "解决什么", "value": "大文件每次改动都要整份重写，又慢又费硬盘；拆小之后只重写被改到的那一小片。"},
            {"name": "存储形态", "value": "词库集合 = 一个目录 + 若干可独立打开的分片 .xirang + 一份 manifest.xirang（清单）。每个分片单独就是一棵可读的树。"},
            {"name": "怎么切分片", "value": "由「分片根判定器」决定哪些节点是分片的根：root（顶层根，默认）/ name:<名> / depth:<层号> / marker:<标记>；选定根之后，它下面的整棵子树都归入同一分片。"},
            {"name": "怎么写", "value": "写 = 只往目标分片末尾追加一条「同一编号的新记录」（后写覆盖）；读 = 按追加顺序取每个编号的最后一条，就是当前视图。"},
            {"name": "怎么删", "value": "删 = 追加一条同编号、名字与值都为空的记录（留空槽位）；整条删除 = 删掉那个分片文件 + 清单里的登记。"},
            {"name": "合并", "value": "追加会越积越多；用 xr compact 把「同一编号只留最后一条」折叠回基版（可手动，也可设阈值自动）。"},
            {"name": "清单长什么样", "value": "manifest.xirang 的根节点名是「分片清单」（普通名字，不是 @ 辅助节点），挂 @protocol 与 @rule；分片条目统一挂在「分片」容器下（名 = 分片根名，值 = 文件名）。"},
            {"name": "代价", "value": "分片根若不在顶层，它的父指针会跨文件，单看一个分片时校验可能报 E011；完整读取要连着集合一起看。"},
        ]},
        {"name": "catalog-v1 本机目录", "children": [
            {"name": "一句话", "value": "在本机维护一份「编号 → 文件」的目录，用来跨库找节点：在常用词库里看到「猫」，不必先知道它在生物词库，直接由目录定位过去。"},
            {"name": "解决什么", "value": "节点编号（UUID）全局唯一，但引用里只存编号、不存文件名；要跨文件找，就得知道「这个编号在哪个文件」。"},
            {"name": "放在哪", "value": "用户级单个文件，默认 ~/.config/xirang/catalog.idx；可用环境变量 XIRANG_CATALOG 改。它只是缓存，随时可删、可重建。"},
            {"name": "什么时候写", "value": "读写命令默认都会顺带维护（读命令可用 --no-index 或 XIRANG_INDEX=off 关闭）；也可显式用 xr catalog scan 扫描。"},
            {"name": "怎么写才快", "value": "目录里「文件段」（路径 + 指纹 + 条数）在前、按编号排好序的条目段在后；查一个编号用磁盘二分，只读十几条小记录，不把整个目录读进内存。"},
            {"name": "怎么判过期", "value": "每条记录带文件的尺寸与修改时间，对不上就重扫该文件；写入用「临时文件 + 改名」原子替换，失败静默（不影响命令）。"},
            {"name": "同一个编号在多个文件", "value": "不是冲突，是正常现象：常用词库和生物词库各留一份自己的「猫」，各挂自己关心的孩子；查一个编号会拿到所有含它的文件。"},
            {"name": "读时取并集", "value": "按编号读节点时，把孩子跨相关文件凑成并集（相关文件 = 命令行给的文件 + 目录里含该编号的文件）；顺序固定为「文件顺序 + 文件内追加序」，同一孩子编号只算一次（来源取最先出现的文件）。"},
            {"name": "来源只在内存", "value": "每条结果带一个「来自哪个文件」的来源标记，只活在读取现场；不写进 .xirang，内核与保存 / 导出都不多这个字段。"},
            {"name": "什么时候要人裁决", "value": "只有同一个编号、节点自身（名字 / 值）不一样时才需要裁决：xr catalog check 列出这些编号（连同两边各自的孩子作为判断依据），xr catalog check --sync <编号> --base <文件> 把别处该节点的名字 / 值改成与基准一致（孩子一律不动）。"},
            {"name": "代价", "value": "目录会随「读过的文件」累积；它属于本机、跨项目，不随数据文件分发。"},
        ]},
        {"name": "XRIDX 侧车索引", "children": [
            {"name": "一句话", "value": "在每个 .xirang 旁边放一份小索引（<文件>.xirang.idx），记录「每个节点在第几个字节」，让读取直接跳过去，不用从头扫。"},
            {"name": "解决什么", "value": "文件里的节点是一个接一个顺序堆放的，没有现成目录；想直接跳到某个节点，就得先记下它在文件里的位置。"},
            {"name": "里面有什么", "value": "根块（根编号 → 偏移 + 长度）、归属块（节点编号 → 所属根）、反向块（谁引用了我）、父子块（父编号 → 孩子编号 + 偏移 + 长度）；四块都按编号排序，用磁盘二分查找。"},
            {"name": "什么时候失效", "value": "源文件尺寸或修改时间变了就重建；解码后发现编号对不上也会重建。它只是缓存，删掉会自动重建。"},
        ]},
    ]},
    {"name": "内核边界", "value": "内核只增不改：节点四属性、7 大类标记永不复用；协议只改变「树怎么组织、怎么存」，不改节点本身。"},
]

PROTOCOL_HISTORY = [
    ("v1.0", "协议登记定稿：shard-v1（分片词库）、catalog-v1（本机目录）、XRIDX 侧车索引。"),
]


def main():
    p = argparse.ArgumentParser(description="生成息壤原生自举 .xirang 文件")
    p.add_argument("--only", default="all",
                   help="只生成指定文件（逗号分隔：kernel/template/version/errors），默认 all")
    args = p.parse_args()
    only = set(x.strip() for x in args.only.split(",") if x.strip()) if args.only != "all" else set()

    def want(name):
        return not only or name in only

    if want("kernel"):
        write_file(
            ROOT / "spec" / "息壤文档.xirang",
            root_name="息壤",
            root_note="息壤内核规范的原生自举：用节点语言描述节点语言自己。",
            root_source=f"{REPO}/blob/main/README.md",
            protocol="native-spec",
            history=KERNEL_HISTORY,
            tree=KERNEL_TREE,
        )
    if want("template"):
        write_file(
            ROOT / "spec" / "模板.xirang",
            root_name="模板",
            root_note="模板层规范的原生自举。",
            root_source=f"{REPO}/blob/main/spec/模板.md",
            protocol="native-spec",
            history=TEMPLATE_HISTORY,
            tree=TEMPLATE_TREE,
        )
    if want("version"):
        write_file(
            ROOT / "spec" / "版本规范.xirang",
            root_name="版本规范",
            root_note="版本规范的原生自举。",
            root_source=f"{REPO}/blob/main/spec/版本规范.md",
            protocol="native-spec",
            history=VERSION_HISTORY,
            tree=VERSION_TREE,
        )
    if want("errors"):
        write_file(
            ROOT / "errors" / "错误列表.xirang",
            root_name="错误列表",
            root_note="错误列表的原生自举。",
            root_source=f"{REPO}/blob/main/errors/错误列表.md",
            protocol="native-spec",
            history=ERROR_HISTORY,
            tree=ERROR_TREE,
        )
    if want("protocol"):
        write_file(
            ROOT / "spec" / "协议.xirang",
            root_name="协议",
            root_note="协议层的原生自举：用节点语言描述「树怎么组织、怎么存」。",
            root_source=f"{REPO}/blob/main/spec/协议.md",
            protocol="native-spec",
            history=PROTOCOL_HISTORY,
            tree=PROTOCOL_TREE,
        )


if __name__ == "__main__":
    main()
