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
- `--yes`: confirm an action that a guard stopped (template-definition edits, `tmpl rm`, `import` over a non-empty file, `blob-export` over an existing file, `history prune`, `index drop/forget`).
- **For AI agents**: `xr-mcp` exposes the same operations as structured tools (identical capabilities and guards, plus confinement to allowed roots) — see [MCP](MCP.en.md).
- **Writes are append-only**: changing one word appends a few records to the end of the file (milliseconds), instead of rewriting the whole file. So the file grows slowly and one node id may have several records (readers take the last one); the first edit under a root adds an `@protocol = append-v1` marker there. Details and costs: see "Append-only writes" under Writing.
- **For files ≥ 20 MB the two caches move to the background**: index registration and local-catalog registration no longer block the command — it returns first and prints a note (`XIRANG_INDEX_MAINTENANCE=off` disables background maintenance; `XIRANG_INDEX=off` disables the local catalog entirely). A cache catching up a few seconds later costs nothing.
- `XIRANG_INDEX_MODE`: `workspace` (default, one ledger per workspace) / `sidecar` (the older per-file `.idx`); switching requires rebuilding the corresponding index.
- `XIRANG_INDEX_MAINTENANCE`: default `auto` — after a command finishes, if the index log exceeds 30% of the base blocks, it spawns a background process to compact; `off` disables it.
- `XIRANG_INDEX_COMPACT_RATIO` / `XIRANG_INDEX_COMPACT_MIN_BYTES`: compaction trigger ratio and minimum base size (defaults 0.30 / 1000000), for tuning and tests.
- `XIRANG_WORKSPACE`: the workspace root (decides where `.xirang-index/` lives); defaults to the directory of the data file.
- `XIRANG_CATALOG`: location of the local catalog file (default `~/.config/xirang/catalog.idx`).
- `xr ws` does **not** register the files you name into the local catalog (that would read every file in full); use `xr catalog scan` for that.
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

### `xr index <subcommand> [paths…] [--json] [--verbose] [--stale] [--dry-run] [--yes] [--sample N] [--deep]`
Workspace index maintenance (by default the **workspace ledger** — three ledgers under `<workspace>/.xirang-index/`: locate / relations / reverse).
The index is an **implementation-layer cache**: safe to delete, rebuildable, never touches the `.xirang` files themselves and holds no unique data.

| Subcommand | What it does |
|---|---|
| `status [--verbose]` | Overview: block/entry counts and base-vs-log size per ledger, generation, file and node totals, files whose fingerprint doesn't match, whether compaction is advised, total index size; `--verbose` also lists the first rows of the file table |
| `files [--stale]` | Per file: entry count, generations (block baseline / latest), fingerprint match, still on disk |
| `update` | **Incremental repair**: rescans only files whose fingerprint changed (self-healing), then drops entries for deleted files |
| `rebuild` | Full rebuild (defaults to every `.xirang` in the workspace) |
| `compact` | Merge the log back into blocks (new block names → atomic manifest swap → old blocks deleted) |
| `check [--sample N] [--deep]` | Consistency check: locate and read N sampled ids (default 5); `--deep` also rescans each file and compares entry counts |
| `gc` | Drop entries for files that no longer exist (space is reclaimed by the next compaction) |
| `drop --yes` | **Delete the whole index directory** (data files are untouched) |
| `forget <files…> --yes` | Remove the given files from the ledger (data files are kept) |
| `unlock` | Clear the writer lock (prints the pid inside and whether it is still running) |
| `path` | Print the absolute path of the index directory |

After a data file is written, the write path appends log records itself (blocks are never edited in place); when the log exceeds 30% of the base blocks, `status` advises a compaction.
With `XIRANG_INDEX_MODE=sidecar` this degrades to the older per-file `.idx` (`status` / `rebuild` / `gc` are available there).
**Destructive subcommands (`drop` / `forget`) require `--yes`**; every subcommand accepts `--dry-run` to show what it would do without doing it.
All subcommands support `--json` (camelCase fields) for scripts and GUIs.

```bash
xr index status                     # current state of this workspace's index
xr index files --stale              # which files were changed outside xr
xr index update                     # repair them incrementally (self-healing)
xr index rebuild lexicon.shards     # or rescan everything
xr index compact                     # merge the log into blocks
xr index check                       # sampled consistency check
xr index drop --yes                  # delete the index (data untouched)
```

### `xr history <file> <node-id>`
Show a node's `@history` snapshots.

```bash
xr history data.xirang <nodeID>
```

### `xr history prune <file> <node-id> [--keep N] [--before <ISO-prefix>] [--dry-run] [--yes]`
Trim history: keep only the most recent N snapshots (default 20), optionally only dropping those older than a given time.

**Why**: history is the one thing that makes a single file grow without bound — measured on one node edited 50 times: 8850 bytes / 103 nodes with history, versus 2498 bytes / 1 node with `--no-history` (each edit adds 2 nodes: the snapshot plus `@replaced`). Those snapshots also go into the index.

**Careful**: the trimmed snapshots are a **real structural deletion**, so you lose that much rollback ability:

- without `--yes` it only prints "would drop N, keep M" and exits with code 2;
- `--dry-run` rehearses without touching the file;
- the snapshots that are kept remain usable with `xr revert`.

```bash
xr history prune data.xirang <nodeID> --keep 5            # see how much would go first
xr history prune data.xirang <nodeID> --keep 5 --yes      # actually trim
xr history prune data.xirang <nodeID> --before 2026-01-01 --yes   # only trim before 2026
```

The same node id in other files is **not affected**: pruning only touches this node's history in this one file.

### `xr diff <a.xirang> <b.xirang> [--json]`
Compare two files by node ID (added/removed/changed, with before/after values).

```bash
xr diff old.xirang new.xirang
xr diff old.xirang new.xirang --json
```

## Writing

### Append-only writes (append-v1)

A write command does not edit "that line in the file" — it **appends a new record with the same node id** to the end of the file, and readers take the last record per id. That is where "changing one word writes a few dozen bytes" comes from (on a 3.47-million-node file, one word went from over ten seconds to milliseconds).

Three things you will notice:

1. **The file grows**: several records accumulate for the same id. Fold them once in a while with `xr compact <file>` (see "Fold / compact").
2. **The first edit adds one auxiliary node**: the root you edited gets an `@protocol = append-v1` marker (added once). Without it, `xr validate` would report those normal duplicate ids as an E002 error — see `spec/协议.md`.
3. **Old content cannot be corrupted**: appending never rewrites existing bytes; a crash mid-write leaves only a trailing fragment (F015), which reads ignore with a note.
4. **No full load**: single-node writes (`set` / `rename` / `rm` / `link`) read that one record by id straight from the workspace ledger — about **10 ms** for one word in a 3.47-million-node file. If the ledger is missing or out of sync it falls back to loading the whole file; the result is the same, just slower.

Exceptions: whole-file `import`, `history prune` and `tmpl rm` still rewrite the whole file — they are supposed to make it actually smaller, or need to express "the record is really gone" (which appending cannot express). `--no-history` only affects history recording, not this rule.

### `xr batch <file|collection-dir> <list-file|-> [--no-history] [--dry-run] [--json] [--yes] [--allow-missing-target]`

Submit **a batch** of changes at once (for migrations of hundreds of thousands of records). The list is **JSONL**: one change per line; blank lines and `#` comments are skipped; a whole-file JSON array works too; `-` reads from stdin.

```text
{"op":"set","id":"<id>","value":"new value"}   set the value (string / number / boolean / null)
{"op":"set","id":"<id>","ref":"<target id>"}   set the value to a reference
{"op":"link","id":"<id>","to":"<target id>"}   change a reference (same thing)
{"op":"rename","id":"<id>","name":"new name"}  rename
{"op":"rm","id":"<id>"}                        delete (empty the name and value)
{"op":"rm_subtree","id":"<id>"}                empty the whole subtree (every descendant)
```

- **Validate everything first, then write**: if any line is invalid (id not in this file, protected template definition, name too long, …) **nothing is applied**, and the error names the line.
- **One process, one write, one ledger registration** — no per-change process start. Changing the same id several times in one batch writes one final record and one history entry.
- `--dry-run` validates only: it reports how many nodes would change and writes nothing.
- `--no-history` skips `@history` (recommended for migrations: otherwise each change adds two extra history nodes).
- Only **existing** nodes; to bulk **create**, use `xr import --append`.
- **Reference targets are checked**: a `link` / `ref` target must be findable in **this workspace (including other shards) or the local catalog**, otherwise the whole batch is refused with the line number; pass `--allow-missing-target` to write a dangling reference on purpose.
- **Unregistered files work**: a collection copied to a clean directory has no ledger yet — the command registers it in full on the spot (single-node writes do the same), so a migration never stalls at step one.
- **One list can span several files**: when the target is a **collection directory**, ops are dispatched to the shard holding each id, and each shard is validated-then-written; the output reports per file. Shards have no cross-file transaction, but the failure message says which files succeeded, so re-running is easy.

`rm` vs `rm_subtree` (both "empty", never a physical delete — that is what append-only means): `rm` empties just that one node and leaves its children in place; `rm_subtree` empties every node in the subtree (each gets a `@history` snapshot when history is on, so `xr revert` can restore them one by one). To keep a subtree intact but out of the main view, `rename` its top node to an `@`-prefixed name (e.g. `@已合并`) — nothing is changed.

Measured (187 MB / 3.47 million nodes, release): **400,000 reference changes in 105 seconds** (one command per change used to take hours), growing the file by about 27 MB; afterwards `xr validate` reports 0 errors and `xr index compact` takes 4.4 s. Cost scales with "number of changes + number of distinct ancestors involved"; for batches this size, `--dry-run` first and compact afterwards.

> **Before a large batch, check the ledger log**: when it is large (say over 64 MB) the command prints a hint — run `xr index compact` first and the batch will be much faster (a large log never changes results, it only costs extra reads when locating nodes).

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

### Fold / compact `xr compact <file|dir> [--all] [--dry-run]`

Fold overlay records back into a base version (one record per node id). Two forms:

- **`<file>`**: folds duplicate records for the same id **inside this one file only**; it never merges across files and never merges ids — "the same id in several files" is cross-file identity and has nothing to do with this command. This is the slimming exit for append-only writes.
- **`<dir>`**: folds the shards of a collection (`--all` also rewrites shards with no revisions).

Output reports "records N → M, bytes X → Y"; when there are no duplicates it says plainly that **not a single byte was written**. `--dry-run` rehearses only. Folding never changes any readable result (readers already take the last record), so it is **not destructive and needs no `--yes`**.

```bash
xr compact big.xirang            # fold this one file (records / bytes before and after)
xr compact big.xirang --dry-run  # see how much would be folded, without touching the file
xr compact lexicon.shards --all  # fold every shard in a collection
```

> Don't confuse this with `xr index compact`, which compacts the **ledger's own** log (`.xirang-index/`) and has nothing to do with data files.

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
