# 息壤 CLI（`xr`）

> [English](CLI.en.md)

`xr` 是息壤（XiRang）的命令行工具，读写 `.xirang` 文件、按结构/名字/值查询、建模板并批量实例化。

**这是什么。** 一个在终端里敲命令用的小程序。不确定有什么命令时，先跑 `xr --help` 看清单；下面每条命令都配了可以直接照抄的例子。

**最短上手。**

```bash
xr info 数据.xirang     # 先看看文件里有什么：节点数、根节点数
xr tree 数据.xirang     # 把整棵树按层级打印出来
```

## 构建

```bash
export RUSTUP_HOME="$PWD/.rustup" CARGO_HOME="$PWD/.cargo" PATH="$PWD/.cargo/bin:$PATH"
cargo build --manifest-path rust/Cargo.toml -p xirang-cli
# 产出 rust/target/debug/xr（及 xr-mcp，MCP server，见 docs/MCP.md）
```

## 约定

- `<file>`：`.xirang` 路径，几乎所有命令的第一参数。
- 写命令 `xr new/set/rename/rm/link/fill` 的 `<file>` 也可传 **shard 词库目录**（见「分片词库」节），内部定位到目标分片只改那一个。
- **I/O 约定**：数据走 stdout，错误/提示走 stderr，退出码 `0`=成功、`1`=校验失败、`2`=用法/运行错误。命令可安全批处理、供程序解析。
- `--json`：结构化输出（替代人类可读文本），供程序/大模型消费，后面不再重复说明。
- `--no-history`：写操作不记 `@history`/`@created`（批量创建、初始数据用）。
- `--yes`：确认执行被护栏拦下的操作（改模板定义、`tmpl rm`、`import` 覆盖非空文件、`blob-export` 覆盖已有文件、`history prune`、`index drop/forget`）。
- **给 AI 代理用**：`xr-mcp` 把同一套操作暴露成结构化工具（能力与护栏完全一致，另外限定在允许目录内），见 [MCP](MCP.md)。
- **写文件是「只追加」的**：改一个词只往文件末尾补几条记录（毫秒级），不是整份重写。所以文件会缓慢变大、同一个编号会有多份记录（读的时候取最后一条），首次编辑某个根时会在它下面补一条 `@protocol = append-v1` 声明——细节与代价见「只追加写」一节。
- **大文件（≥ 20 MB）的两本缓存会转后台**：索引登记与本机目录登记都不再挡住命令，命令先返回并打一行提示（`XIRANG_INDEX_MAINTENANCE=off` 关闭后台整理，`XIRANG_INDEX=off` 干脆不用本机目录）。缓存晚几十秒跟上没有数据风险。
- `XIRANG_INDEX_MODE`：`workspace`（默认，工作区统一索引）/ `sidecar`（旧的每文件侧车索引）。切换后需要重建对应索引。
- `XIRANG_INDEX_MAINTENANCE`：默认 `auto`——命令跑完（结果已打印）后，若索引日志超过主干 30%，会**另起一个后台进程**去压实；设 `off` 关闭。
- `XIRANG_INDEX_COMPACT_RATIO` / `XIRANG_INDEX_COMPACT_MIN_BYTES`：压实的触发比例与最小主干体积（默认 0.30 / 1000000），供调参与测试。
- `XIRANG_WORKSPACE`：工作区根（决定 `.xirang-index/` 放哪）；默认 = 数据文件所在目录。
- `XIRANG_CATALOG`：本机目录文件位置（默认 `~/.config/xirang/catalog.idx`）。
- `xr ws` **不会**把命令行里点名的文件登记进本机目录（登记要把每个文件整份读一遍）；需要登记用 `xr catalog scan`。
- `--no-index`：读命令默认会把读到的文件登记进**本机目录**（见「本机目录」节），此标志单次关闭；也可用环境变量 `XIRANG_INDEX=off` 全局关闭。
- 路径寻址：`名/子名/孙名`（`/` 分隔），相对某子树根。

## 读取

### `xr info <file>`
文件摘要。

```bash
xr info 数据.xirang         # 格式版本、头文本、节点数、根节点数
```

### `xr tree <file> [--node <id>] [--skip-aux] [--ids] [--head N] [--depth N] [--no-pager]`
缩进树视图；`--ids` 在每个节点后显示其编号（UUID）；`--head N` 只打印前 N 个节点；`--depth N` 只展开到第 N 层（根算第 1 层）。截断提示走 stderr，stdout 保持干净、可直接管道。

```bash
xr tree 数据.xirang                       # 全部树
xr tree 数据.xirang --node <节点ID>       # 只看某子树
xr tree 数据.xirang --skip-aux            # 跳过 @ 辅助节点
xr tree 数据.xirang --ids                 # 带上节点编号，便于后续 set/rm/link
xr tree 大词库.xirang --depth 2           # 只看前两层，先摸骨架
xr tree 大词库.xirang --head 200          # 只打印前 200 个节点（百万级文件别整棵打）
```

### `xr cat <file> [--head N] [--ids] [--skip-aux] [--force] [--no-pager]`
**扁平视图**：按文件里的存放顺序，一行一个节点（`名字 = 值`），没有缩进——想「像看文本文件一样」浏览时用它。

```bash
xr cat 数据.xirang                        # 全部节点，一行一个
xr cat 数据.xirang --head 200             # 只看前 200 行
xr cat 数据.xirang --ids                  # 每行后面带节点编号
```

> **护栏**：不带 `--head` 时，如果文件超过 10 万节点会直接拒绝（避免上百 MB 刷屏），提示改用 `--head`；确实要全打再加 `--force`。

> **自动分页**：`tree` / `cat` 在**输出到终端**且内容较长时会自动交给 `$PAGER`（默认 `less -R`）翻页；管道或重定向时行为不变（脚本友好）。`--no-pager` 可单次关闭。

### `xr find <file> <pattern> [--json]`
按节点名/文本值搜（子串匹配）。

```bash
xr find 词库.xirang 灯                    # 名或值含「灯」的节点
xr find 词库.xirang 灯 --json
```

### `xr match <file> --root <名>|--shape-of <节点ID>|--template <名> [--where 路径=值] [--tree] [--json]`
按结构/名字/值过滤树。三选一锚点，可叠 `--where` 值约束。这是**发现**自由数据的主命令。

> `--root <名>` 是**按节点名**锚定候选子树（匹配该名字的任意节点，**不要求是顶层根**）；要严格按顶层根筛，用 `--shape-of` 或先 `xr tree --ids` 看结构。

```bash
xr match 词库.xirang --root 词条 --where 词形=灯 --json     # 所有名为「词条」且词形=灯 的树
xr match 词库.xirang --shape-of <样例节点ID>                 # 拓扑同该样例的树（发现）
xr match 词库.xirang --template 词条 --where 词形=火         # 模板实例里筛（已登记）
```

### `xr refs <file> <node-id>`
查看引用边（出/入）。

```bash
xr refs 图.xirang <节点ID>     # 出边（我引用谁）+ 入边（谁引用我）
```

### `xr ws <node-id> <file1> [file2…] [--only <file>] [--json]`
按编号跨文件解析：默认列出该编号在**各相关文件**里的每一份（名字旁标来源文件），孩子取**并集**（每条标来源），并显示它引用了谁 / 谁引用了它。

相关文件 = 命令行给的文件 + 本机目录里含该编号的文件；目录不可用（`--no-index` / `XIRANG_INDEX=off`）时退化为只用命令行给的文件。同一编号在多个文件里是正常现象，各留各的孩子。

```bash
xr ws <节点ID> a.xirang b.xirang   # 两份都列出，孩子取并集，每条标来源
xr ws <节点ID> a.xirang --only a.xirang   # 只看 a.xirang 里的那一份（孩子也只来自它）
```

### `xr index <子命令> [路径…] [--json] [--verbose] [--stale] [--dry-run] [--yes] [--sample N] [--deep]`
工作区索引维护（默认走「工作区统一索引」：`<工作区>/.xirang-index/` 下的三本台账——定位 / 关系 / 反向）。
索引是**实现层缓存**：可整体删除、可重建，不改动 `.xirang` 本体，也不含任何独家数据。

| 子命令 | 作用 |
|---|---|
| `status [--verbose]` | 总览：三本台账的块数/条目数/主干与日志体积、代数、文件与编号总数、指纹不符的文件、是否建议压实、索引目录总体积；`--verbose` 再列文件表前几条 |
| `files [--stale]` | 逐文件：条目数、代号（块基线/最新）、指纹是否一致、磁盘上在不在 |
| `update` | **增量修复**：只重扫指纹变了的文件（自愈），顺带清掉已删除文件的条目 |
| `rebuild` | 全量重建（缺省扫工作区全部 `.xirang`） |
| `compact` | 把日志合并回块（新块写新名字 → 原子换 manifest → 删旧块） |
| `check [--sample N] [--deep]` | 一致性校验：抽样 N 个编号（默认 5）定位并读出；`--deep` 再对每个文件重扫比对条目数 |
| `gc` | 清掉已不存在的文件条目（下次压实后真正回收） |
| `drop --yes` | **删除整个索引目录**（数据文件不受影响） |
| `forget <文件…> --yes` | 把指定文件的条目从台账移除（数据文件保留） |
| `unlock` | 清掉写者锁（报出锁里的 pid 及是否还在运行） |
| `path` | 打印索引目录的绝对路径 |

数据文件写完后，索引由写路径自动追加日志（不原地改块）；体积超过主干 30% 时 `status` 会提示压实。
`XIRANG_INDEX_MODE=sidecar` 时退化为旧的每文件 `.idx`（`status` / `rebuild` / `gc` 可用）。
**破坏性命令（`drop` / `forget`）必须加 `--yes`**；任何命令都可加 `--dry-run` 先看会做什么（不动手）。
所有子命令都支持 `--json`（camelCase 字段），供脚本与 GUI 消费。

```bash
xr index status                     # 看当前工作区索引状态
xr index files --stale              # 哪些文件被外部改过
xr index update                     # 增量修好它们（自愈）
xr index rebuild 词库.shards         # 或整库重扫
xr index compact                     # 压实日志
xr index check                       # 一致性抽查
xr index drop --yes                  # 删掉索引（数据不动）
```

### `xr history <file> <node-id>`
查看某节点的 `@history` 快照。

```bash
xr history 数据.xirang <节点ID>
```

### `xr diff <a.xirang> <b.xirang> [--json]`
按节点编号对比两个文件（增/删/改，含值前后）。

```bash
xr diff 旧.xirang 新.xirang
xr diff 旧.xirang 新.xirang --json
```

## 写入

### 只追加写（append-v1）

写命令改的不是「文件里那一行」，而是往文件**末尾追加一条同编号的新记录**；读的时候每个编号取最后一条。这是「改一个词只写几十字节」的来源（347 万节点的文件上，改一个词从十几秒降到毫秒级）。

你会看到三件事：

1. **文件会变大**：同一个编号攒下多份记录。攒太多就用 `xr compact <文件>` 折叠一次（见「折叠 / 压实」）。
2. **首次编辑会多一个辅助节点**：被编辑的那棵树根下会补一条 `@protocol = append-v1` 声明（只加一次）。没有它，`xr validate` 会把这种正常的重复编号当成 E002 错误——`spec/协议.md` 里有完整说明。
3. **改坏不了旧内容**：追加写不会原地改字节；写到一半崩了只会留下尾部残片（F015），读取时忽略并提示。
4. **不用整份载入**：单节点写（`set` / `rename` / `rm` / `link`）按编号用台账**直读那一条记录**，347 万节点的文件上改一个词约 **10 毫秒**；台账还没有、或与文件对不上时自动退回「整份载入」，结果一样，只是慢一点。

例外：`import` 的**整文件替换**、`history prune`（裁剪留痕）、`tmpl rm`（删模板）这几类是「整份重写」——它们本来就要让文件真的变小，或者要表达「记录真的没了」（追加写表达不了）。`--no-history` 只影响留痕，不影响这条规则。

### `xr new <file> <parent|nil> <name> [value] [--no-history]`
新增节点。`parent` 为 `nil` 表示建根。文件不存在时自动新建。

```bash
xr new 数据.xirang nil 根
xr new 数据.xirang <父节点ID> 灯 2046
```
> 在非辅助父节点下新增会改变该子树「形状码」（影响按结构检索），会有一行软提示；`--yes` 跳过。

### `xr set <file> <node-id> <value> [--no-history]`
改值（写 `@history` 快照）。

```bash
xr set 数据.xirang <节点ID> 新值
```

### `xr rename <file> <node-id> <新名字> [--no-history]`
改名。编号不变、引用不断，旧名字进 `@history`；`--no-history` 则不记。空名字会被拒绝（要清空名字请用 `xr rm`）。

```bash
xr rename 数据.xirang <节点ID> 新名字
```

### `xr rm <file> <node-id>`
删除（清空名+值、留空槽位、旧值进 `@history`）。已是空节点则不记录。

```bash
xr rm 数据.xirang <节点ID>
```

### `xr link <file> <from-id> <to-id> [--no-history]`
建引用边（from 的值 = 指向 to 的引用）。

```bash
xr link 图.xirang <甲ID> <乙ID>
```

### `xr copy <file> <node-id> <parent|nil> [--blank] [--no-history]`
复制一棵子树（换新 UUID）；子树内引用重指副本、子树外保持原目标。

```bash
xr copy 词库.xirang <词条ID> nil                 # 完整克隆（带 @history）
xr copy 词库.xirang <词条ID> nil --blank --no-history   # 只有结构（清空值、无 @history），做骨架
```

### `xr fill <file> <root-id> <名/路径=值>… [--no-history]`
按名字/路径给一棵子树赋值（相对 root）。

```bash
xr fill 词条.xirang <词条ID> 词形=灯 词义/01/释义=照明器具
```

### `xr revert <file> <node-id>`
回滚到最近 `@history` 快照。

```bash
xr revert 数据.xirang <节点ID>
```

### `xr history prune <file> <node-id> [--keep N] [--before <ISO前缀>] [--dry-run] [--yes]`
裁剪留痕：只保留最近 N 条快照（默认 20），可选再要求「早于某时刻」。

**为什么需要**：留痕是唯一会让单文件越用越大的东西——实测同一个节点连续改 50 次，默认写法文件 8850 字节、103 个节点，而 `--no-history` 只有 2498 字节、1 个节点（每改一次多 2 个节点：快照 + `@replaced`）。这些快照还会一起进索引。

**注意**：被裁掉的快照是**真正的结构删除**，裁了就失去那部分回滚能力，所以：

- 不带 `--yes` 时只打印「会丢掉多少条、保留多少条」并退出码 2
- `--dry-run` 先预演（不动文件）
- 保留的那几条仍可用 `xr revert` 回滚

```bash
xr history prune 数据.xirang <节点ID> --keep 5            # 先看会裁多少
xr history prune 数据.xirang <节点ID> --keep 5 --yes      # 真裁
xr history prune 数据.xirang <节点ID> --before 2026-01-01 --yes   # 只裁 2026 年以前的
```

跨文件的同一编号**不受影响**：裁剪只动这一个文件里这个节点的留痕，不会去重、也不会碰别的文件。

## 分片词库（shard）

一个「大 `.xirang`」可无损拆成「目录 + 若干分片 `.xirang` + `manifest.xirang`」，之后写操作只动目标分片，不再重写整个词库。

### `xr collection split <file> --rule <规则> [--out <dir>]`
按判定器把整库无损拆成多个分片 + 清单。规则：`root`（顶层根，默认）/ `name:<名>` / `depth:<k>` / `marker:@分片`。默认输出 `<file>.shards/`。

```bash
xr collection split 词库.xirang --rule root
xr collection split 词库.xirang --rule name:词条 --out 词库.shards
```

### `xr collection list <dir>`
列出词库集合的判定器与各分片。

```bash
xr collection list 词库.shards
```

### 折叠 / 压实 `xr compact <文件|目录> [--all] [--dry-run]`

把「覆盖记录」折叠回基版（同一个编号只留最后一条）。两种形态：

- **`<文件>`**：只折叠**这一个文件内部**同一编号的多份记录；**不跨文件、不合并编号**——「同一个编号出现在多个文件里」是跨文件身份，与这个命令无关。这是「只追加写」攒多了之后的瘦身出口。
- **`<目录>`**：折叠词库目录里各个分片（`--all` 连没有修订的分片也重写一遍）。

输出会报「记录多少 → 多少、字节多少 → 多少」；**没有重复记录时明说「一个字节都没改」**（这时真的不写）。加 `--dry-run` 只预演。折叠不改变任何读得到的结果（读的人本来就取最后一条），所以**它不是破坏性操作，不需要 `--yes`**。

```bash
xr compact 大文件.xirang            # 折叠这一个文件（记录 / 字节前后都打出来）
xr compact 大文件.xirang --dry-run  # 先看看会折掉多少，不动文件
xr compact 词库.shards --all        # 折叠词库里所有分片
```

> 不要把它和 `xr index compact` 弄混：那个压的是**台账自己**的日志（`.xirang-index/`），跟数据文件无关。

### 在词库里直接写
`xr new/set/rename/rm/link/fill` 的 `<file>` 也可传词库目录，内部定位到目标分片只改那一个：

```bash
xr new 词库.shards nil 新词条
xr set 词库.shards <节点ID> 新值
```

## 本机目录（catalog）

用户级单文件索引（默认 `~/.config/xirang/catalog.idx`，`XIRANG_CATALOG` 可覆盖），记录 `UUID → 文件路径`，用于跨库连接。**读命令默认也会维护它**（`--no-index` 或 `XIRANG_INDEX=off` 关闭）；只写该目录文件，不改动 `.xirang`，写失败静默跳过。

### `xr catalog scan [路径...]`
扫描文件 / 目录（缺省当前目录）进本机目录。

```bash
xr catalog scan 词库.shards 生物词库.xirang
```

### `xr catalog list`
列出已登记的文件与 UUID 数量。

### `xr catalog check`
列出**同一编号、但节点自身（名字 / 值）不一致**的编号。同一编号在多个文件里是正常现象，**不算冲突**；只有自身内容对不上时才需要人来裁决。每条都会附带**两边各自的孩子列表**作为判断依据（孩子不同属正常，不参与判定）。

```bash
xr catalog check
```

### `xr catalog check --sync <uuid> --base <文件>`
以某个文件为基准，把**其它文件**里该节点的名字 / 值同步成与基准一致（会改写那些文件并落盘）；**孩子一律不动**（孩子是各库自己的细节）。

```bash
xr catalog check --sync 550e8400-… --base 常用词库.xirang
```

### `xr catalog forget <路径>`
从目录移除一个文件（文件本身不动）。

### `xr catalog trash <路径>`
把文件移入回收站（可恢复）并从目录移除。

### 跨库解析
`xr ws` 默认跨文件取并集：同名编号在多个文件里各算各的，孩子凑并集。传入文件里没有的目标，会自动回退查本机目录：例如从常用词库定位到生物词库里的同一 UUID。

## 模板与实例

### `xr tmpl add <file> <name> [--from-json <sample>]`
创建一棵**模板定义**：一根标 `@模板`(空) 的树（自由根），其普通子节点 = 模板结构（来自样例 JSON）。模板定义受保护（只能经 `xr tmpl` 改）。

```bash
xr tmpl add 词库.xirang 词条 --from-json sample.json
# sample.json：{"词形":"灯","词频":2046,"词义":{"释义":"照明器具","词性":"名词"}}
```

### `xr tmpl list <file>`
列出所有模板（标 `@模板`(空) 的树）及其实例数。

```bash
xr tmpl list 词库.xirang     # 词条 <uuid>（2 实例）
```

### `xr tmpl rm <file> <name> [--yes]`
受保护删除模板定义（连同其所有实例），需 `--yes`。

```bash
xr tmpl rm 词库.xirang 词条 --yes
```

### `xr import <file> --template <name> <data.json> [--under <parent|nil>]`
按模板批量实例化：`data.json` 是记录数组，每条 → 一棵**实例树**（根标 `@实例` + `@模板`→模板，按名字填进模板结构、缺失留空）；实例根可自由挂在任意父节点下（`--under` 指定父，缺省=自由根）。

```bash
xr import 词库.xirang --template 词条 data.json                     # 实例作为自由根
xr import 词库.xirang --template 词性 data.json --under <某词条节点>  # 实例嵌套挂到父下（如词性）
# data.json：[{"词形":"火","词频":7,"词义":{"释义":"燃烧物","词性":"名词"}}, …]
```

### `xr instances <file> <name>`
列出某模板的所有实例树（按 `@模板`(引用) 关联定位，无论实例挂在何处）。

```bash
xr instances 词库.xirang 词条
```

## 二进制块

### `xr blob-import <file> <parent|nil> <src>`
把文件导入为二进制块节点。

```bash
xr blob-import 资产.xirang nil logo.png
```

### `xr blob-export <file> <node-id> <dest>`
导出二进制块为文件。**目标文件已存在时拒绝覆盖**，确认要覆盖请加 `--yes`（这条护栏与 MCP 侧一致）。

```bash
xr blob-export 资产.xirang <节点ID> out.png
```

### `xr blob-info <file> <node-id>`
二进制块信息 / 文本预览。

```bash
xr blob-info 资产.xirang <节点ID>
```

## 校验

### `xr validate <file>`
结构校验（E/R 错误）。退出码：`0` 通过、`1` 有错误。

```bash
xr validate 数据.xirang     # 校验通过：0 错误
```

## 格式转换

### `xr export <file> <json|yaml|xml|md> [--subtree <id>]`
导出（`md` 有损、只用导出）。

```bash
xr export 数据.xirang json
xr export 数据.xirang json --subtree <节点ID>    # 只导出某子树
```

### `xr import <file> <json|yaml|xml> <source>`
导入（`json` 需是 `xr export json` 的格式；`data.json` 若为记录数组且想按模板实例化，用 `--template`，见上）。

这是**整文件替换**：目标文件里已有节点时会被拦下，确认覆盖请加 `--yes`；想保留原内容请改用 `--append` / `--template`。

```bash
xr import 数据.xirang yaml data.yaml
```
