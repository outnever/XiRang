# XiRang MCP server (`xr-mcp`)

> [中文版](MCP.md)

`xr-mcp` is a [Model Context Protocol](https://modelcontextprotocol.io) (MCP) server that exposes `xr`'s ability to read and write `.xirang` files as structured tools, for consumption by AI agents (vibecoding tools).

**What it is.** A small service for AI assistants: if your editor or assistant supports MCP, it calls these tools on its own to read and write XiRang files — you usually don't type any command by hand. It and `xr` are one set of capabilities with two doors: `xr` is for people, `xr-mcp` is for programs. **Capabilities, guards, and errors all come from the same implementation** (`rust/cli/src/ops.rs`), so the two doors can't drift apart.

## Start

```bash
export RUSTUP_HOME="$PWD/.rustup" CARGO_HOME="$PWD/.cargo" PATH="$PWD/.cargo/bin:$PATH"
cargo build --manifest-path rust/Cargo.toml -p xirang-cli
rust/target/debug/xr-mcp --root /your/data/dir
# runs over stdio; connect its stdout to your MCP client
```

**`--root` is the security boundary — configure it.** Repeatable; or set `XIRANG_MCP_ROOTS` (path-separator separated). Relative paths resolve against the **first** root, and the resolved real path (including `..` and symlinks) must stay inside one of the roots, otherwise it is refused. With neither set, the default root is the process working directory, which still blocks escaping paths.

### Wiring it into a client

Most MCP clients start servers from a JSON config shaped like this (use your own paths):

```json
{
  "mcpServers": {
    "xirang": {
      "command": "/abs/path/rust/target/release/xr-mcp",
      "args": ["--root", "/your/data/dir"]
    }
  }
}
```

`xr-mcp --help` prints the same instructions.

## Tools (10, grouped by domain)

| Tool | Actions | Purpose |
|---|---|---|
| `context` | — | Start here: allowed roots, version, actions per tool |
| `file_info` | — | file summary (format version / node count / root count) |
| `file_validate` | — | structural validation (E/R errors) |
| `file_diff` | — | compare two files (added/removed/changed, by node id) |
| `tree` | `layout=tree\|flat` | tree view; `node`/`depth`/`limit`/`skip_aux`; `limit` defaults to 200 |
| `query` | `find` / `match` / `instances` / `refs` / `history` | search; match by structure/name/template; instances; reference edges; `@history` snapshots |
| `node` | `create` / `set` / `rename` / `remove` / `link` / `copy` / `fill` / `revert` | edit nodes |
| `template` | `define` / `list` / `instantiate` / `remove` | define templates, list them, batch-instantiate (optionally `under` a parent), remove |
| `convert` | `export` / `import` / `append` | export to `json`/`yaml`/`xml`/`md`, whole-file import, append nested JSON as a subtree |
| `blob` | `import` / `export` / `info` | blob values: read in, write out, inspect |

Each tool carries an `inputSchema` (JSON Schema) the agent can fill by. Tool results arrive in `content[].text` as a JSON string.

## Destructive actions need an explicit `force`

MCP has no "stop and ask" turn, so these actions are refused unless you pass `force: true`, with a reason in the error:

- editing/removing nodes inside a **template definition** (`node` `set`/`rename`/`remove`, or `copy` onto a template definition)
- `template` `remove` (takes the template's instances with it)
- `node` `create` under a nonexistent parent, `link` to a missing or cross-library target
- `convert` `import` over a **non-empty** file, `blob` `export` over an **existing** file

Refusals return `isError: true` with a body of `{"error":{"kind":"guarded","message":…,"hint":…}}`. `kind` is one of `invalid_argument` / `not_found` / `guarded` / `path_denied` / `unsupported` / `internal`. Data-level problems still carry `X` codes such as `E`/`R` (see `errors/错误列表.md`).

## What MCP deliberately does not cover

Machine-local state stays in the `xr` CLI: `catalog` (the local catalog), `index` (sidecar index), `collection` / `compact` (shard libraries), `ws` (cross-file view).

MCP **never writes the local catalog**. The consequence matters: an MCP-only session does not register files into the catalog, so `xr ws` will not find them afterwards — run `xr catalog scan` first if you need cross-file resolution. Shard library directories (`.xirang` directories) always return `unsupported` over MCP.

## Old tool names → new tool names

Grouped by domain since 0.2; the old names are gone:

| Old | New |
|---|---|
| `info` | `file_info` |
| `validate` | `file_validate` |
| `diff` | `file_diff` |
| `find` / `match` / `instances` | `query` (via `action`) |
| `rename_node` | `node` (`action=rename`) |
| `import_template` | `template` (`action=instantiate`) |
| `head_nodes` | `tree` (`layout=flat` + `limit`) |

## Relation to `xr`

`xr` is the human-facing CLI; `xr-mcp` is the protocol interface for programs/LLMs. Both share the core library (`xirang-core`) and the operations layer (`rust/cli/src/ops.rs`): the same operation gives the same result on either door, with the same guards. Only the entry rules differ — the CLI lets *you* decide (`--yes`), MCP makes the *caller* declare intent (`force`) and confines paths to the allowed roots.
