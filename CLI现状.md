# CLI 现状与性能（给桌面端 session 的移交说明）

> 写于 2026-09-26，对应 main 的 `086190d`。目的：让接手桌面端的 session **按现在的 CLI 能力设计**，
> 不要按记忆里的旧机制重复实现或回退已有改动。所有数字都是实测，不是估计。

## 一、一句话概括

CLI（`xr`）与 MCP（`xr-mcp`）已经重构成**同一份共享操作层 + 两个薄入口**：

- 唯一实现层：`rust/cli/src/ops.rs`（能力、护栏、错误都在这里）；
- `main.rs` 只做参数解析 + 文本渲染；`mcp.rs` 只做工具定义 + JSON 分发；
- 桌面端该接的就是这一层（或等价地调 `xr` 子命令），**不要再写第二份写入逻辑**。

目前本仓库的桌面端 `app/src-tauri/src/main.rs` 是**直接调 core 的 `store.save`**，
create / rename / update / remove / revert 五条各写一份——这正是「两条实现路径」的来源。

## 二、现在有什么（能力清单）

**写 / 读 / 查**

| 类别 | 命令 |
|---|---|
| 读 | `info`、`tree`（`--node/--skip-aux/--ids/--head/--depth/--no-pager`）、`cat`、`validate`、`find`、`match`（`--root/--shape-of/--template/--where/--json/--tree`）、`instances`、`refs`、`ws`、`diff`、`history` |
| 写 | `new`、`set`、`rename`、`rm`、`link`、`copy`、`fill`、`revert`、`import`（整文件替换 / `--append` / `--template`） |
| 模板 | `tmpl add/list/rm`、`import --template --under` |
| 转换 | `export`（json/yaml/xml/md）、`import` |
| 二进制块 | `blob-import`、`blob-export`、`blob-info` |
| 留痕 | `history`、**`history prune`**（`--keep N` / `--before <ISO前缀>` / `--dry-run` / `--yes`） |
| 本机目录 | `catalog scan/list/check/forget/trash` |
| 分片词库 | `collection split/list`、`compact <dir>`（**只接受词库目录**）、`index` |

**索引管理（11 条，全部支持 `--json`）**

`xr index status [--verbose]` · `files [--stale]` · `update` · `rebuild` · `compact` ·
`check [--sample N] [--deep]` · `gc` · `drop [--yes]` · `forget <文件…> [--yes]` · `unlock` · `path`

**环境变量**

| 变量 | 作用 |
|---|---|
| `XIRANG_INDEX_MODE` | `workspace`（默认，工作区台账）/ `sidecar`（旧的每文件 `.idx`，回退用） |
| `XIRANG_INDEX_MAINTENANCE` | 默认 `auto`：命令跑完后按阈值**分离后台进程**压实；`off` 关闭 |
| `XIRANG_INDEX_COMPACT_RATIO` / `..._MIN_BYTES` | 压实触发比例（默认 0.30）与最小主干体积（默认 1 MB） |
| `XIRANG_WORKSPACE` | 指定工作区根（决定 `.xirang-index/` 放哪） |
| `XIRANG_CATALOG` | 本机目录文件位置 |
| `XIRANG_INDEX` | `off` 关闭读/写时维护本机目录 |

**护栏（两个入口一致）**：改模板定义、`tmpl rm`、`import` 覆盖非空文件、`blob-export` 覆盖已有文件、
`history prune`、`index drop/forget` —— CLI 要 `--yes`、MCP 要 `force: true`；不带就只报「会做什么」并退出码 2。

## 三、写路径现在实际怎么走（这段最关键）

```
xr set <file> <id> <值>
  └─ ops::set_value
       ├─ load_target          整份载入（或按编号载入所在分片）
       ├─ guard_editable       模板定义保护
       ├─ store.update(...)    改值 + 往该节点 @history 追加「快照 + @replaced」
       ├─ save(...)            ← Store::save：**整份重写数据文件**
       ├─ hooks.on_save(...)   登记本机目录（catalog）
       └─ ops::update_index
            └─ wsidx::append_file(工作区, 文件)
                 ├─ 重扫该文件**全部**节点
                 ├─ 给该文件写一条「新代号」记录（旧代号条目全部作废）
                 └─ 把该文件的定位/关系/反向条目整份追加进三本日志
```

**结论（必须记住）**：单文件编辑的代价 = **整份重写数据文件** + **重扫登记该文件全部节点**，
与「改了几个节点」无关，只与「这个文件里有多少节点」成正比。这就是下面 54 秒的成因。

## 四、性能现状（实测）

**60 万节点级对比**（词典型夹具；规模 1 万 / 10 万 / 100 万节点，取 100 万档）

| 场景 | 工作区台账（默认） | 每文件侧车（回退） |
|---|---|---|
| 点查（任意编号） | **30 µs** | 2556 µs（85×） |
| 父子（孩子并集） | **69 µs** | 2755 µs（40×） |
| 反向（谁引用我） | **20 µs** | 1925 µs（99×） |
| 整树读取 | **169 µs** | 10969 µs（65×） |
| 建索引 | 553 ms | 276 ms |
| 索引体积 | 124 MB（≈ 数据的 2.4 倍） | 100 MB |
| 单条写入（含重扫登记） | 6.7 ms（该文件 2232 节点） | 0.9 ms |
| 压实 | 567 ms | — |

侧车是 O(文件数)，台账是 O(1)：文件越多差距越大（5 个文件时两者持平）。

**入口开销**：100 万节点 / 445 文件，`xr ws` **7.6 ms**（优化前约 90 ms）；
MCP 常驻会话里一次查询 0.4 ms。整份载入（内存内查询 8–28 µs）是性能上界，代价是启动 115 ms / 常驻约 110 MB。

**留痕代价与裁剪**：同一节点改 30 次 → 文件 6350 字节、63 个节点；`xr history prune --keep 5` → 3243 字节（-49%）。
改 50 次对比：留痕 8850 字节 / 103 节点，`--no-history` 2498 字节 / 1 节点（每改一次 +2 节点）。

**与其它存储对照**（同一份 30 万节点数据）：息壤台账点查 27 µs；SQLite 建索引后点查 7.6 µs、单条更新 14.8 µs（快 460×）；
JSON 需整份载入 128 ms。体积：息壤数据 15.3 MB / JSON 32 MB / SQLite 63 MB。

**桌面端 54 秒的拆解**（你们的观测）：

- 整份重写 179 MB 数据：约 5 秒；
- `wsidx::append_file` 重扫 347 万节点并整份重登索引：约 49 秒——日志每次写入约 460–540 MB
  （定位 52 B/节点 + 关系 68 B/边 + 反向 36 B/引用边）。
- 所以「只把数据文件改成追加写」最多省掉那 5 秒，**必须同时让索引只登记变化**才有效果。

## 五、对你们那份两步计划的评审

### 步骤 1 · 单文件改成 `append_changes` 追加写 —— 方向对，但有三处必须一起处理

1. `shard::append_changes(path, before, after)` 需要「改前 / 改后」两个 Store —— `ops` 里**已经有** before 快照
   （分片写入路径在用），直接复用，不要另写。
2. 追加之后，同一个编号在文件里会有**多份记录**，读路径必须承担「后写覆盖」（fold）语义。
   `Store::load` / 内部索引是后写覆盖 ✔，但**几乎所有 CLI 写命令都是「整份载入 → 整份重写」**——
   第一次无关的小编辑就会把追加产生的多份记录折叠掉（等于隐性压实）。所以追加写要和这些命令的写回方式一起改。
3. **「压实」的出口现在不存在于单文件**：`xr compact` 只接受**分片词库目录**（它要读 `<目录>/manifest.xirang`）。
   你们计划里写的「复用 `xr compact <文件>`」目前不成立。要么补一个单文件压实命令
   （你们之前否掉的 `xr fold` 就是它），要么明确接受「单文件里长期留多份旧记录、越用越大」。

### 步骤 2 · `append_tail` 只登记新增的那一段 —— 必须同时改台账的有效性规则

现在 `append_file` 会为该文件写一条**新代号**记录，规则是「**该文件的旧代号条目一律作废**」。
所以「只追加新尾部条目」会让旧条目全部失效——`xr ws` 直接查不到东西（静默丢数据，最难查的一类 bug）。

要做得对，必须把规则改成二者之一：

- **按 (编号, 文件) 后写覆盖** + 显式的删除记录（节点被 `rm` 时补一条 tombstone）；
- 或者**给每条条目带代号**，读数时只认最新代号。

这是整轮改动里风险最高的一处。做它之前请先把分支 `codex/workspace-index-bench`（`c0f5c5c`）上的
`core/tests/index_parity.rs`（三后端四类查询逐条对拍）与 `index_faults.rs`（故障演练）拿回来当护栏。

### 步骤 3 · 桌面端接 CLI —— 方向正确

- 写动作改调 `xr new/set/rename/rm/link/copy/fill/revert`，或直接调 `xirang-core` 的同一层；
- 压实、留痕、索引维护**一律复用现有命令**，一行都不要新写：
  `xr index compact`（索引日志）、`xr compact <词库目录>`（分片折叠）、`xr history prune`（留痕）、
  `xr index update/rebuild/status/files/drop`；
- 桌面端**不要再实现一遍** `--yes` / `force` 那套护栏——它在共享层里，去掉就是回退。

## 六、不要重做 / 不要回退（清单）

1. 不要再写第二份 MCP 工具：现有 10 个（`context/file_info/file_validate/file_diff/tree/query/node/template/convert/blob`）已与 CLI 对齐。
2. 不要把索引退回 `.xirang.idx` 侧车：默认是工作区台账，`sidecar` 只是回退开关。
3. 不要新加「整份重扫登记」以外的索引入口而不改有效性规则（见 §5 步骤 2）。
4. 不要自己写留痕裁剪：用 `xr history prune`（`--dry-run` 可预演）。
5. 不要自己写索引压实：`xr index compact`；分片折叠用 `xr compact <dir>`。
6. 不要在单文件上调用 `xr compact`——它只吃词库目录。
7. 不要去掉 `--yes` / `force` 护栏，也不要让默认路径绕过共享层。
8. 不要把「同一编号出现在多个文件里」当成冲突去重：那是**跨文件身份**的保证
   （本机目录的 `catalog check` 只在「节点自身名字/值不一致」时才要人裁决）。
9. 不要假设索引一定新鲜：指纹不符时读路径会自动回退整份载入并打印原因；修复用 `xr index update`。
10. 不要在写路径中途留下半成品：写路径的改动要一次做完并跑完全量回归（`cargo test` + `pytest`）。

## 七、可直接照抄的验收命令

```bash
xr index status --json            # 索引规模、日志占比、是否需要压实
xr index files --stale            # 哪些文件被外部改过（需要 xr index update）
xr index check --sample 50        # 抽样定位+读出；--deep 再逐文件比对条目数
xr index path                     # 索引目录在哪
xr history prune <file> <id> --keep 5 --dry-run   # 留痕裁剪预演
XIRANG_INDEX_MODE=sidecar xr ws <id> <file…>      # 与旧索引对比，结果必须一致
```

## 八、回归现状（改这块之前请知道）

- main（`086190d`）上常驻防护有三条，覆盖最容易回退的地方：
  `core/tests/wsidx_min.rs`（三后端一致 + 索引坏了必须报 F012 并回退）、
  `cli/tests/index_guard.rs`（drop/forget 要 `--yes`、预演不动手、JSON 可解析）、
  `cli/tests/history_prune_min.rs`（裁剪默认拦下、预演不改文件、确认后变小且能回滚）+ Python 侧跨语言一致性。
- 更重的测试脚手架（基准工具、夹具生成器、三后端全量对拍、故障演练）**不随仓库发布**，
  但都在分支 `codex/workspace-index-bench`（`c0f5c5c`）上。**动写路径或台账之前，建议先把它取回来。**
- 全量回归：`cargo test --manifest-path rust/Cargo.toml`（9 个测试目标）与 `python3 -m pytest tests/`（72 条）。
