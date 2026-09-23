# 息壤 MCP server（`xr-mcp`）

> [English](MCP.en.md)

`xr-mcp` 是[模型上下文协议](https://modelcontextprotocol.io)（MCP）服务，把 `xr` 的查询/导入能力暴露成结构化工具，供 AI 代理（vibecoding 软件）消费。

**这是什么。** 一个给 AI 助手用的小服务：如果你的编辑器或助手支持 MCP，它会自己调用这些工具去读写息壤文件，你一般不需要手动敲命令。它和 `xr` 是一套本事、两个入口——`xr` 给人用，`xr-mcp` 给程序用。

## 启动

```bash
export RUSTUP_HOME="$PWD/.rustup" CARGO_HOME="$PWD/.cargo" PATH="$PWD/.cargo/bin:$PATH"
cargo run --manifest-path rust/Cargo.toml --bin xr-mcp
# 走 stdio；把 stdout 接给你的 MCP 客户端
```

## 工具

| 工具 | 作用 |
|---|---|
| `info` | 文件摘要（节点数/根数） |
| `tree` | 一棵子树的结构（嵌套，含名字/值/引用） |
| `find` | 按名字/文本值搜节点 |
| `match` | 按结构/名字/值匹配树（`root`/`shape_of`/`template` 三选一 + `where` 值约束） |
| `instances` | 某模板的实例（按 `@模板`(引用) 定位，可挂任意父下） |
| `validate` | 结构校验（E/R 错误） |
| `diff` | 两文件对比（增/删/改） |
| `import_template` | 按模板批量实例化（写入文件） |
| `rename_node` | 给节点改名（写入文件）；编号不变、引用不断，旧名字进 `@history` |
| `head_nodes` | 按存放顺序返回前 n 个节点（扁平列表，默认 200）；大文件时替代整棵 `tree` |

每个工具带 `inputSchema`（JSON Schema），代理按 schema 填参即可。工具返回 `content[].text` 为 JSON 字符串。

## 与 `xr` 的关系

`xr` 是给人用的命令行；`xr-mcp` 是给程序/大模型用的协议接口。二者共用核心库（`xirang-core`）与操作层（`rust/cli/src/ops.rs`），行为一致。
