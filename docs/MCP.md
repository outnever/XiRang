# 息壤 MCP server（`xr-mcp`）

> [English](MCP.en.md)

`xr-mcp` 是[模型上下文协议](https://modelcontextprotocol.io)（MCP）服务，把 `xr` 读写 `.xirang` 的能力暴露成结构化工具，供 AI 代理（vibecoding 软件）消费。

**这是什么。** 一个给 AI 助手用的小服务：如果你的编辑器或助手支持 MCP，它会自己调用这些工具去读写息壤文件，你一般不需要手动敲命令。它和 `xr` 是一套本事、两个入口——`xr` 给人用，`xr-mcp` 给程序用。**能力、护栏、错误都来自同一份实现**（`rust/cli/src/ops.rs`），所以不会出现「一边拦、一边不拦」。

## 启动

```bash
export RUSTUP_HOME="$PWD/.rustup" CARGO_HOME="$PWD/.cargo" PATH="$PWD/.cargo/bin:$PATH"
cargo build --manifest-path rust/Cargo.toml -p xirang-cli
rust/target/debug/xr-mcp --root /你的数据目录
# 走 stdio；把 stdout 接给你的 MCP 客户端
```

**`--root` 是安全边界，建议一定要配。** 可重复给多个；也可用环境变量 `XIRANG_MCP_ROOTS`（用路径分隔符分隔）。相对路径基于**第一个** root 解析；解析后的真实路径（含 `..` 与符号链接）必须落在某个 root 内，否则拒绝。两个都没配时，默认 root = 进程工作目录，仍然挡住越界路径。

### 接到客户端

大多数 MCP 客户端用一份 JSON 配置来启动服务端，形状如下（把路径换成你自己的）：

```json
{
  "mcpServers": {
    "xirang": {
      "command": "/绝对路径/rust/target/release/xr-mcp",
      "args": ["--root", "/你的数据目录"]
    }
  }
}
```

`xr-mcp --help` 里有同样的说明。

## 工具（10 个，按领域分组）

| 工具 | 动作 | 作用 |
|---|---|---|
| `context` | — | 先看这里：允许目录、版本、各工具支持的动作清单 |
| `file_info` | — | 文件摘要（格式版本 / 节点数 / 根数） |
| `file_validate` | — | 结构校验（E/R 错误） |
| `file_diff` | — | 两文件对比（增/删/改，按节点编号） |
| `tree` | `layout=tree\|flat` | 树视图；可给 `node`/`depth`/`limit`/`skip_aux`；`limit` 默认 200 |
| `query` | `find` / `match` / `instances` / `refs` / `history` | 搜索、按结构/名字/模板匹配、实例、引用边、`@history` 快照 |
| `node` | `create` / `set` / `rename` / `remove` / `link` / `copy` / `fill` / `revert` / `prune_history` / `batch` | 改节点；`batch` 一次提交一批改动（`ops` 数组，或 `batch` 的 JSONL 文本；可带 `dry_run`） |
| `template` | `define` / `list` / `instantiate` / `remove` | 建模板、列模板、按模板批量建实例（可 `under` 挂到某父下）、删模板 |
| `convert` | `export` / `import` / `append` | `json`/`yaml`/`xml`/`md` 导出、整文件导入、把嵌套 JSON 追加成子树 |
| `blob` | `import` / `export` / `info` | 二进制块：读入、写出、查看 |

每个工具带 `inputSchema`（JSON Schema），代理按 schema 填参即可。工具结果在 `content[].text` 里，是一段 JSON 字符串。

## 危险操作要显式 `force`

MCP 没有「停下来问你一下」的回合，所以下面这些操作不带 `force: true` 就直接拒绝，并告诉你原因：

- 改 / 删**模板定义**里的节点（`node` 的 `set`/`rename`/`remove`，或 `copy` 落到模板定义上）
- `template` 的 `remove`（会连同该模板的所有实例一起删）
- `node` 的 `create` 挂到不存在的父节点、`link` 指向不存在或跨库的目标
- `convert` 的 `import` 覆盖**非空**文件、`blob` 的 `export` 覆盖**已有**文件

被拒绝时返回 `isError: true`，正文是 `{"error":{"kind":"guarded","message":…,"hint":…}}`。`kind` 取值：`invalid_argument` / `not_found` / `guarded` / `path_denied` / `unsupported` / `internal`。数据本身的校验错误仍带 `E`/`R` 等 `X` 错误码（见 `errors/错误列表.md`）。

## 不在 MCP 里的东西

「你这台电脑的状态」这类操作留在 `xr` CLI：`catalog`（本机目录）、`index`（工作区台账的 status / files / update / rebuild / compact / check / gc / drop / forget / unlock / path）、`collection` / `compact`（分片词库，与折叠数据文件里的覆盖记录）、`ws`（跨文件视图）。

MCP **不写本机目录**。后果要说清楚：纯 MCP 的会话不会把文件登记进 catalog，之后 `xr ws` 就不会自动找到它们；要跨文件解析，先跑一次 `xr catalog scan`。分片词库目录（`.xirang` 目录）在 MCP 一律返回 `unsupported`。

工作区索引（`.xirang-index/` 下的三本台账）**由写路径自动维护**：CLI 与 MCP 共用同一份操作层，写数据后都会往索引追加日志；重建与压实用 CLI 的 `xr index ...`。

## 旧工具名 → 新工具名

0.2 起按领域重新分组，旧名不再保留：

| 旧 | 新 |
|---|---|
| `info` | `file_info` |
| `validate` | `file_validate` |
| `diff` | `file_diff` |
| `find` / `match` / `instances` | `query`（用 `action` 区分） |
| `rename_node` | `node`（`action=rename`） |
| `import_template` | `template`（`action=instantiate`） |
| `head_nodes` | `tree`（`layout=flat` + `limit`） |

## 与 `xr` 的关系

`xr` 是给人用的命令行；`xr-mcp` 是给程序/大模型用的协议接口。二者共用核心库（`xirang-core`）与操作层（`rust/cli/src/ops.rs`）：同一操作两条路结果一致，护栏也一致。差别只在**入口规则**：CLI 让你决定（`--yes`），MCP 让调用方显式声明（`force`），并且路径被限制在允许目录内。
