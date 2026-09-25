# XiRang CLI (`xr`)

> [中文版](CLI.md)

`xr` is the XiRang command-line tool: it reads/writes `.xirang` files, queries by structure/name/value, and builds templates with batch instantiation.

**What it is.** A small program you run by typing commands in a terminal. If you are unsure what is available, run `xr --help` first. Every command below comes with a copy-pasteable example.

**Quickest start.**

```bash
xr info data.xirang     # first, see what is inside: node count, root count
xr tree data.xirang     # print the whole tree by level
```

## Build

```bash
export RUSTUP_HOME="$PWD/.rustup" CARGO_HOME="$PWD/.cargo" PATH="$PWD/.cargo/bin:$PATH"
cargo build --manifest-path rust/Cargo.toml -p xirang-cli
# produces rust/target/debug/xr (and xr-mcp, an MCP server — see docs/MCP.en.md)
```

## Conventions

- `<file>`: a `.xirang` path, the first argument of almost every command.
- The write commands `xr new/set/rename/rm/link/fill` may also take a **shard collection directory** as `<file>` (see "Shard collections"), targeting the one shard that needs changing.
- **I/O convention**: data goes to stdout, errors/notes go to stderr; exit codes are `0`=success, `1`=validation failed, `2`=usage/runtime error. Commands are safe to pipeline and to parse programmatically.
- `--json`: structured output (instead of human text), for programs/LLMs.
- `--no-history`: write operations do not record `@history`/`@created` (for batch creation / initial data).
- `--yes`: confirm an action that a guard stopped (template-definition edits, `tmpl rm`, `import` over a non-empty file, `blob-export` over an existing file).
- **For AI agents**: `xr-mcp` exposes the same operations as structured tools (identical capabilities and guards, plus confinement to allowed roots) — see [MCP](MCP.en.md).
- `--no-index`: read commands index the files they touch into the **local catalog** by default (see "Local catalog"); this flag disables it for one run, and `XIRANG_INDEX=off` disables it globally.
- Path addressing: `name/child/grandchild` (`/`-separated), relative to a given subtree root.

## Reading

### `xr info <file>`
File summary.

```bash
xr info data.xirang          # format version, header text, node count, root count
```

### `xr tree <file> [--node <id>] [--skip-aux] [--ids] [--head N] [--depth N] [--no-pager]`
Indented tree view; `--ids` appends each node's ID (UUID); `--head N` prints only the first N nodes; `--depth N` expands only N levels (root = level 1). Truncation notes go to stderr so stdout stays pipeable.

```bash
xr tree data.xirang                       # all trees
xr tree data.xirang --node <nodeID>       # one subtree
xr tree data.xirang --skip-aux            # skip `@` aux nodes
xr tree data.xirang --ids                 # include node IDs for later set/rm/link
xr tree big.xirang --depth 2              # top two levels only
xr tree big.xirang --head 200             # first 200 nodes only (don't dump millions)
```

### `xr cat <file> [--head N] [--ids] [--skip-aux] [--force] [--no-pager]`
**Flat view**: one node per line (`name = value`) in storage order, with no indentation — use this when you want to browse the file as plain text.

```bash
xr cat data.xirang                        # every node, one per line
xr cat data.xirang --head 200             # first 200 lines only
xr cat data.xirang --ids                  # append each node's ID
```

> **Guard**: without `--head`, files above 100,000 nodes are refused (to avoid dumping hundreds of MB); add `--force` if you really mean it.

> **Auto-paging**: `tree` / `cat` page through `$PAGER` (default `less -R`) when writing to a terminal and the output is long. Pipes and redirections are unaffected (script-friendly). Use `--no-pager` to turn it off for one run.

### `xr find <file> <pattern> [--json]`
Search by node name / text value (substring match).

```bash
xr find lexicon.xirang lamp                    # nodes whose name or value contains "lamp"
xr find lexicon.xirang lamp --json
```

### `xr match <file> --root <name>|--shape-of <nodeID>|--template <name> [--where path=value] [--tree] [--json]`
Filter trees by structure/name/value. Pick one anchor, optionally add `--where` value constraints. This is the main command for **discovering** free-form data.

> `--root <name>` anchors **by node name** (any node with that name becomes a candidate subtree root; it need not be a top-level root). To restrict to top-level roots, use `--shape-of`, or inspect the structure first with `xr tree --ids`.

```bash
xr match lexicon.xirang --root entry --where word=lamp --json   # trees named "entry" with word=lamp
xr match lexicon.xirang --shape-of <sampleNodeID>               # same topology as the sample (discovery)
xr match lexicon.xirang --template entry --where word=fire      # among registered instances
```

### `xr refs <file> <node-id>`
Show reference edges (outgoing/incoming).

```bash
xr refs graph.xirang <nodeID>    # outgoing (what I point to) + incoming (what points to me)
```

### `xr ws <node-id> <file1> [file2…] [--only <file>] [--json]`
Resolve a node by id across files: by default it lists every copy of that id in each **relevant file** (with the source file shown next to the name), builds the **union** of children (each tagged with its source), and shows what it references and who references it.

Relevant files = the files you pass on the command line + the files the local catalog says contain that id. When the catalog is unavailable (`--no-index` / `XIRANG_INDEX=off`), it falls back to the files you passed. The same id in several files is normal; each file keeps its own children.

```bash
xr ws <nodeID> a.xirang b.xirang   # list both copies, union their children, tag each with its source
xr ws <nodeID> a.xirang --only a.xirang   # only the copy in a.xirang (and only its children)
```

### `xr index <file1> [file2…]`
Rebuild/refresh the sidecar index and print a summary (an implementation-layer `.xirang.idx` cache; never modifies the `.xirang` itself).

```bash
xr index lexicon.xirang graph.xirang   # per file: root count / node count / edge count / new or reused
```

### `xr history <file> <node-id>`
Show a node's `@history` snapshots.

```bash
xr history data.xirang <nodeID>
```

### `xr diff <a.xirang> <b.xirang> [--json]`
Compare two files by node ID (added/removed/changed, with before/after values).

```bash
xr diff old.xirang new.xirang
xr diff old.xirang new.xirang --json
```

## Writing

### `xr new <file> <parent|nil> <name> [value] [--no-history]`
Add a node. `parent` = `nil` to create a root. Creates a new file if it does not exist.

```bash
xr new data.xirang nil root
xr new data.xirang <parentNodeID> lamp 2046
```
> Adding under a non-aux parent changes that subtree's "shape code" (affects structure search); a soft note is printed; `--yes` suppresses it.

### `xr set <file> <node-id> <value> [--no-history]`
Change a value (records an `@history` snapshot).

```bash
xr set data.xirang <nodeID> "new value"
```

### `xr rename <file> <node-id> <new name> [--no-history]`
Rename a node. The id stays the same and references keep pointing at it; the old name goes into `@history` (skip with `--no-history`). An empty name is rejected (use `xr rm` to blank a node).

```bash
xr rename data.xirang <nodeID> newName
```

### `xr rm <file> <node-id>`
Delete (empty out name+value, keep an empty slot, old value into `@history`). Already-empty nodes record nothing.

```bash
xr rm data.xirang <nodeID>
```

### `xr link <file> <from-id> <to-id> [--no-history]`
Create a reference edge (from's value points to to).

```bash
xr link graph.xirang <aID> <bID>
```

### `xr copy <file> <node-id> <parent|nil> [--blank] [--no-history]`
Copy a subtree (new UUIDs); references inside are re-pointed to the copy, references outside keep the original target.

```bash
xr copy lexicon.xirang <entryID> nil                 # full clone (keeps @history)
xr copy lexicon.xirang <entryID> nil --blank --no-history   # structure only, no history
```

### `xr fill <file> <root-id> <name/path=value>… [--no-history]`
Assign values to a subtree by name/path (relative to root).

```bash
xr fill entry.xirang <entryID> word=lamp meaning/01/def=an object
```

### `xr revert <file> <node-id>`
Roll back to the most recent `@history` snapshot.

```bash
xr revert data.xirang <nodeID>
```

## Shard collections

A single large `.xirang` can be losslessly split into "a directory + several shard `.xirang` files + a `manifest.xirang`"; after that, writes touch only the target shard instead of rewriting the whole library.

### `xr collection split <file> --rule <rule> [--out <dir>]`
Losslessly split a whole store into shards + a manifest, using a shard-root predicate. Rules: `root` (top-level root, default) / `name:<name>` / `depth:<k>` / `marker:@shard`. Default output is `<file>.shards/`.

```bash
xr collection split lexicon.xirang --rule root
xr collection split lexicon.xirang --rule name:entry --out lexicon.shards
```

### `xr collection list <dir>`
List a collection's predicate and its shards.

```bash
xr collection list lexicon.shards
```

### `xr compact <dir> [--all]`
Fold a shard's overlay log (same-UUID last-write-wins). `--all` folds every shard; otherwise only shards that have revisions are folded.

```bash
xr compact lexicon.shards --all
```

### Writing into a collection
`xr new/set/rename/rm/link/fill` may also take a collection directory as `<file>`, targeting only the shard that needs changing:

```bash
xr new lexicon.shards nil newEntry
xr set lexicon.shards <nodeID> newValue
```

## Local catalog

A user-level single-file index (default `~/.config/xirang/catalog.idx`, overridable with `XIRANG_CATALOG`) mapping `UUID → file path`, for cross-library lookup. **Read commands also maintain it by default** (disable with `--no-index` or `XIRANG_INDEX=off`); it only writes the catalog file, never the `.xirang`, and write failures are skipped silently.

### `xr catalog scan [paths...]`
Scan files/directories (default: current directory) into the catalog.

```bash
xr catalog scan lexicon.shards biology.xirang
```

### `xr catalog list`
List indexed files and their UUID counts.

### `xr catalog check`
List ids whose **own name/value differs** across files. The same id in multiple files is normal and **not a conflict**; only a mismatch in the node's own content needs a human decision. Each entry also shows **both sides' children lists** as context (differing children are normal and do not count).

```bash
xr catalog check
```

### `xr catalog check --sync <uuid> --base <file>`
Using one file as the base, rewrite the name/value of that node in the **other** files to match the base (those files are rewritten and saved); **children are left untouched** (children are each library's own detail).

```bash
xr catalog check --sync 550e8400-… --base common-words.xirang
```

### `xr catalog forget <path>`
Drop a file from the catalog (the file itself is untouched).

### `xr catalog trash <path>`
Move a file to the trash (recoverable) and drop it from the catalog.

### Cross-library resolution
`xr ws` takes the cross-file union by default: the same id in several files counts in each, with children unioned. When a target is not among the files passed in, it falls back to the local catalog — for example, locating the same UUID in the biology library from a query in the common-words library.

## Templates & instances

### `xr tmpl add <file> <name> [--from-json <sample>]`
Create a **template definition**: a tree whose root is annotated `@模板`(empty) (free root); its ordinary child nodes = the template structure (from the sample JSON). Template definitions are protected (editable only via `xr tmpl`).

```bash
xr tmpl add lexicon.xirang entry --from-json sample.json
# sample.json: {"word":"lamp","freq":2046,"sense":{"def":"an object","pos":"noun"}}
```

### `xr tmpl list <file>`
List all templates (`@模板`(empty)-annotated roots) and their instance count.

```bash
xr tmpl list lexicon.xirang     # entry <uuid>（2 instances）
```

### `xr tmpl rm <file> <name> [--yes]`
Protected delete of a template definition (along with all its instances); requires `--yes`.

```bash
xr tmpl rm lexicon.xirang entry --yes
```

### `xr import <file> --template <name> <data.json> [--under <parent|nil>]`
Batch-instantiate by template. `data.json` is an array of records; each record becomes one **instance tree** (root annotated `@实例` + `@模板`→template, values filled into the template structure by name, missing fields left empty). An instance root can be freely attached under any parent node (`--under`, default = free root).

```bash
xr import lexicon.xirang --template entry data.json                     # instances as free roots
xr import lexicon.xirang --template pos data.json --under <someNode>    # nest an instance under a parent
# data.json: [{"word":"fire","freq":7,"sense":{"def":"burns","pos":"noun"}}, …]
```

### `xr instances <file> <name>`
List all instance trees of a template (located by the `@模板`(reference) association, wherever the instance is attached).

```bash
xr instances lexicon.xirang entry
```

## Blobs

### `xr blob-import <file> <parent|nil> <src>`
Import a file as a blob node.

```bash
xr blob-import assets.xirang nil logo.png
```

### `xr blob-export <file> <node-id> <dest>`
Export a blob node to a file. It **refuses to overwrite an existing file** — add `--yes` if you really want to replace it (same guard as the MCP side).

```bash
xr blob-export assets.xirang <nodeID> out.png
```

### `xr blob-info <file> <node-id>`
Blob info / text preview.

```bash
xr blob-info assets.xirang <nodeID>
```

## Validation

### `xr validate <file>`
Structural validation (E/R errors). Exit code: `0` = pass, `1` = errors.

```bash
xr validate data.xirang     # 校验通过：0 错误
```

## Format conversion

### `xr export <file> <json|yaml|xml|md> [--subtree <id>]`
Export (`md` is lossy, export-only).

```bash
xr export data.xirang json
xr export data.xirang json --subtree <nodeID>    # export only a subtree
```

### `xr import <file> <json|yaml|xml> <source>`
Import. `json` must be the `xr export json` format; for an array of records, use `--template` (see above).

This **replaces the whole file**: if the target already holds nodes you'll be stopped — add `--yes` to overwrite, or use `--append` / `--template` to keep what's there.

```bash
xr import data.xirang yaml data.yaml
```
