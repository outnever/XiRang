# XiRang MCP server (`xr-mcp`)

> [中文版](MCP.md)

`xr-mcp` is a [Model Context Protocol](https://modelcontextprotocol.io) (MCP) server that exposes `xr`'s query/import capabilities as structured tools, for consumption by AI agents (vibecoding tools).

**What it is.** A small service for AI assistants: if your editor or assistant supports MCP, it calls these tools on its own to read and write XiRang files — you usually don't type any command by hand. It and `xr` are one set of capabilities with two doors: `xr` is for people, `xr-mcp` is for programs.

## Start

```bash
export RUSTUP_HOME="$PWD/.rustup" CARGO_HOME="$PWD/.cargo" PATH="$PWD/.cargo/bin:$PATH"
cargo run --manifest-path rust/Cargo.toml --bin xr-mcp
# runs over stdio; connect its stdout to your MCP client
```

## Tools

| Tool | Purpose |
|---|---|
| `info` | file summary (node count / root count) |
| `tree` | one subtree's structure (nested, with name/value/reference) |
| `find` | search nodes by name / text value |
| `match` | match trees by structure/name/value (`root`/`shape_of`/`template` anchor + `where` value constraints) |
| `instances` | a template's instances (located by the `@模板`(reference) association, attachable under any parent) |
| `validate` | structural validation (E/R errors) |
| `diff` | compare two files (added/removed/changed) |
| `import_template` | batch-instantiate by template (writes the file) |
| `rename_node` | rename a node (writes the file); the id stays the same so references survive, old name goes to `@history` |
| `head_nodes` | the first n nodes in storage order (flat list, default 200); use it instead of the whole `tree` on large files |

Each tool carries an `inputSchema` (JSON Schema) the agent can fill by. Tool returns `content[].text` as a JSON string.

## Relation to `xr`

`xr` is the human-facing CLI; `xr-mcp` is the protocol interface for programs/LLMs. Both share the core library (`xirang-core`) and the operations layer (`rust/cli/src/ops.rs`), so their behavior is identical.
