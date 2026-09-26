//! `xr-mcp` 集成测试：进程级跑 MCP server（JSON-RPC over stdio），验证
//! 工具清单、路径策略、护栏、写操作，以及「同一操作 CLI / MCP 结果一致」。

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

/// 一个跑着的 MCP server 会话。
struct Mcp {
    child: Child,
    stdin: ChildStdin,
    out: BufReader<ChildStdout>,
    next_id: u64,
}

impl Mcp {
    fn start(root: &Path) -> Mcp {
        let mut child = Command::new(env!("CARGO_BIN_EXE_xr-mcp"))
            .arg("--root")
            .arg(root)
            .env("XIRANG_CATALOG", catalog_path(root))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("拉起 xr-mcp");
        let stdin = child.stdin.take().unwrap();
        let out = BufReader::new(child.stdout.take().unwrap());
        Mcp { child, stdin, out, next_id: 0 }
    }

    /// 发一条 JSON-RPC 请求，读到对应 id 的响应。
    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        let req = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(self.stdin, "{req}").unwrap();
        self.stdin.flush().unwrap();
        loop {
            let mut line = String::new();
            let n = self.out.read_line(&mut line).unwrap();
            assert!(n > 0, "MCP server 提前退出");
            let v: Value = serde_json::from_str(line.trim()).expect("响应应是 JSON");
            if v.get("id") == Some(&json!(id)) {
                return v;
            }
        }
    }

    /// 调一个工具，成功则返回解析后的结果对象。
    fn call(&mut self, tool: &str, args: Value) -> Value {
        let resp = self.request("tools/call", json!({"name": tool, "arguments": args}));
        let res = &resp["result"];
        assert!(
            !res["isError"].as_bool().unwrap_or(false),
            "调用 {tool} 失败：{}",
            res["content"][0]["text"]
        );
        body(res)
    }

    /// 调一个工具，期望失败，返回错误体 `{kind, message, hint?}`。
    fn call_err(&mut self, tool: &str, args: Value) -> Value {
        let resp = self.request("tools/call", json!({"name": tool, "arguments": args}));
        let res = &resp["result"];
        assert!(
            res["isError"].as_bool().unwrap_or(false),
            "调用 {tool} 本该失败，却成功了：{}",
            res["content"][0]["text"]
        );
        body(res)["error"].clone()
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// 工具结果的正文（content[0].text 里是一段 JSON 字符串）。
fn body(result: &Value) -> Value {
    let text = result["content"][0]["text"].as_str().expect("结果是文本");
    serde_json::from_str(text).expect("正文应是 JSON")
}

/// 每个测试一个独立目录，兼作 `--root`。
fn tmp_root(tag: &str) -> PathBuf {
    let n = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let dir = std::env::temp_dir().join(format!("xr-mcp-{tag}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn catalog_path(root: &Path) -> PathBuf {
    root.join("catalog.idx")
}

/// 跑 `xr` CLI（隔离本机目录，避免污染真实 ~/.config）。
fn xr(root: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_xr"));
    c.current_dir(root);
    c.env("XIRANG_CATALOG", catalog_path(root));
    c
}

fn run(mut cmd: Command) -> (i32, String, String) {
    let out = cmd.output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// 语义视图：不带编号、不带辅助节点的树（用来比较两条路线的结果）。
fn semantic_tree(root: &Path, file: &str) -> String {
    let mut c = xr(root);
    c.args(["tree", file, "--skip-aux", "--no-pager"]);
    let (code, stdout, stderr) = run(c);
    assert_eq!(code, 0, "tree 失败：{stderr}");
    stdout
}

/// 从「已创建：名字 <uuid>」这类输出里取出编号。
fn uuid_from(out: &str) -> String {
    let start = out.find('<').expect("输出里应有 <uuid>");
    let rest = &out[start + 1..];
    let end = rest.find('>').expect("输出里应有 >");
    rest[..end].to_string()
}

#[test]
fn tools_list_is_grouped_and_context_reports_roots() {
    let root = tmp_root("list");
    let mut mcp = Mcp::start(&root);

    let resp = mcp.request("tools/list", json!({}));
    let tools = resp["result"]["tools"].as_array().unwrap().clone();
    let names: Vec<String> =
        tools.iter().map(|t| t["name"].as_str().unwrap().to_string()).collect();
    assert_eq!(
        names,
        vec![
            "context",
            "file_info",
            "file_validate",
            "file_diff",
            "tree",
            "query",
            "node",
            "template",
            "convert",
            "blob"
        ],
        "工具清单应正好是这 10 个"
    );
    for t in &tools {
        assert!(t["inputSchema"]["type"] == "object", "{} 缺 inputSchema", t["name"]);
        assert!(
            !t["description"].as_str().unwrap_or("").is_empty(),
            "{} 缺 description",
            t["name"]
        );
    }

    // context：把允许目录报出来，AI 才知道能操作哪里
    let ctx = mcp.call("context", json!({}));
    let reported: Vec<String> =
        ctx["roots"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().into()).collect();
    assert_eq!(reported.len(), 1);
    assert_eq!(
        Path::new(&reported[0]).canonicalize().unwrap(),
        root.canonicalize().unwrap(),
        "context 应回报 --root"
    );
    assert!(ctx["actions"]["node"].as_array().unwrap().contains(&json!("revert")));

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn path_outside_root_and_shard_dir_are_refused() {
    let root = tmp_root("paths");
    // 造一个允许目录外的文件
    let outside =
        std::env::temp_dir().join(format!("xr-mcp-outside-{}.xirang", std::process::id()));
    std::fs::write(&outside, b"not-read").unwrap();

    let mut mcp = Mcp::start(&root);
    let inside = mcp.call("node", json!({"file": "t.xirang", "action": "create", "name": "根"}));
    assert!(inside["created"].is_string());

    // 相对路径逃逸
    let e = mcp.call_err("file_info", json!({"file": "../escape.xirang"}));
    assert_eq!(e["kind"], "path_denied", "{e}");
    // 绝对路径越界
    let e = mcp.call_err("file_info", json!({"file": outside.to_str().unwrap()}));
    assert_eq!(e["kind"], "path_denied", "{e}");
    // blob 的 dest 一样受限
    let target = std::env::temp_dir().join("xr-mcp-should-not-write");
    let e = mcp.call_err(
        "blob",
        json!({"file": "t.xirang", "action": "export", "node": "x",
               "dest": target.to_str().unwrap()}),
    );
    assert_eq!(e["kind"], "path_denied", "{e}");
    assert!(!target.exists(), "越界写不该发生");

    // 分片词库目录：MCP 不处理（留 CLI）
    std::fs::create_dir_all(root.join("shards.xirang")).unwrap();
    let e = mcp.call_err("file_info", json!({"file": "shards.xirang"}));
    assert_eq!(e["kind"], "unsupported", "{e}");

    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_file(&outside).ok();
}

#[test]
fn node_writes_match_the_cli() {
    let root = tmp_root("parity");
    let mut mcp = Mcp::start(&root);

    // —— MCP 一路：建根、建子（带值）、改值、改名、再赋值 ——
    let root_id = mcp.call(
        "node",
        json!({"file": "mcp.xirang", "action": "create", "name": "词库", "no_history": true}),
    )["created"]
        .as_str()
        .unwrap()
        .to_string();
    let entry_id = mcp.call(
        "node",
        json!({"file": "mcp.xirang", "action": "create", "parent": root_id,
               "name": "词条", "no_history": true}),
    )["created"]
        .as_str()
        .unwrap()
        .to_string();
    mcp.call(
        "node",
        json!({"file": "mcp.xirang", "action": "create", "parent": entry_id,
               "name": "词形", "value": "灯", "no_history": true}),
    );
    let xs_id = mcp.call(
        "node",
        json!({"file": "mcp.xirang", "action": "create", "parent": entry_id,
               "name": "释义", "value": "照明器具", "no_history": true}),
    )["created"]
        .as_str()
        .unwrap()
        .to_string();
    mcp.call(
        "node",
        json!({"file": "mcp.xirang", "action": "set", "node": xs_id, "value": "照明用器具"}),
    );
    mcp.call(
        "node",
        json!({"file": "mcp.xirang", "action": "rename", "node": xs_id, "name": "词义"}),
    );
    mcp.call(
        "node",
        json!({"file": "mcp.xirang", "action": "fill", "root": entry_id,
               "assigns": [{"path": "词形", "value": "火"}], "no_history": true}),
    );

    // —— CLI 一路：同样的操作 ——
    let (c, out, err) = run({
        let mut c = xr(&root);
        c.args(["new", "cli.xirang", "nil", "词库", "--no-history"]);
        c
    });
    assert_eq!(c, 0, "{err}");
    let cli_root = uuid_from(&out);
    let (c, out, err) = run({
        let mut c = xr(&root);
        c.args(["new", "cli.xirang", &cli_root, "词条", "--no-history", "--yes"]);
        c
    });
    assert_eq!(c, 0, "{err}");
    let cli_entry = uuid_from(&out);
    let (c, _, err) = run({
        let mut c = xr(&root);
        c.args(["new", "cli.xirang", &cli_entry, "词形", "灯", "--no-history", "--yes"]);
        c
    });
    assert_eq!(c, 0, "{err}");
    let (c, out, err) = run({
        let mut c = xr(&root);
        c.args(["new", "cli.xirang", &cli_entry, "释义", "照明器具", "--no-history", "--yes"]);
        c
    });
    assert_eq!(c, 0, "{err}");
    let cli_xs = uuid_from(&out);
    let (c, _, err) = run({
        let mut c = xr(&root);
        c.args(["set", "cli.xirang", &cli_xs, "照明用器具"]);
        c
    });
    assert_eq!(c, 0, "{err}");
    let (c, _, err) = run({
        let mut c = xr(&root);
        c.args(["rename", "cli.xirang", &cli_xs, "词义"]);
        c
    });
    assert_eq!(c, 0, "{err}");
    let (c, _, err) = run({
        let mut c = xr(&root);
        c.args(["fill", "cli.xirang", &cli_entry, "词形=火", "--no-history"]);
        c
    });
    assert_eq!(c, 0, "{err}");

    // 两条路的结构 / 名字 / 值应完全一致（编号随机，所以比语义不比字节）
    assert_eq!(
        semantic_tree(&root, "cli.xirang"),
        semantic_tree(&root, "mcp.xirang"),
        "CLI 与 MCP 做同样的操作，结果应一致"
    );

    // 用 CLI 复核 MCP 写出来的文件本身是干净的
    let (code, stdout, _) = run({
        let mut c = xr(&root);
        c.args(["validate", "mcp.xirang"]);
        c
    });
    assert_eq!(code, 0);
    assert!(stdout.contains("0 错误"), "{stdout}");

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn template_flow_and_guarded_removal() {
    let root = tmp_root("tmpl");
    let mut mcp = Mcp::start(&root);

    let def = mcp.call(
        "template",
        json!({"file": "t.xirang", "action": "define", "name": "词条",
               "sample": {"词形": "", "释义": ""}}),
    );
    let tpl_id = def["template"]["id"].as_str().unwrap().to_string();

    // 实例：挂在自由根下
    let inst = mcp.call(
        "template",
        json!({"file": "t.xirang", "action": "instantiate", "template": "词条",
               "data": [{"词形": "灯", "释义": "照明器具"}]}),
    );
    assert_eq!(inst["imported"], 1);

    let list = mcp.call("template", json!({"file": "t.xirang", "action": "list"}));
    let templates = list["templates"].as_array().unwrap();
    assert_eq!(templates.len(), 1);
    assert_eq!(templates[0]["name"], "词条");
    assert_eq!(templates[0]["instances"], 1, "实例数应算上刚建的");

    // 模板定义受保护：不带 force 改不动（CLI 那边也一样拦）
    let e = mcp.call_err(
        "node",
        json!({"file": "t.xirang", "action": "rename", "node": tpl_id, "name": "词目"}),
    );
    assert_eq!(e["kind"], "guarded", "{e}");
    let (code, _, stderr) = run({
        let mut c = xr(&root);
        c.args(["rename", "t.xirang", &tpl_id, "词目"]);
        c
    });
    assert_eq!(code, 2, "CLI 也该拦：{stderr}");
    assert!(stderr.contains("受保护") || stderr.contains("模板定义"), "{stderr}");

    // 显式 force 才能改
    mcp.call(
        "node",
        json!({"file": "t.xirang", "action": "rename", "node": tpl_id,
               "name": "词目", "force": true}),
    );

    // 删模板：无 force → 拦；带 force → 连实例一起删
    let e = mcp.call_err(
        "template",
        json!({"file": "t.xirang", "action": "remove", "template": "词目"}),
    );
    assert_eq!(e["kind"], "guarded", "{e}");
    let removed = mcp.call(
        "template",
        json!({"file": "t.xirang", "action": "remove", "template": "词目", "force": true}),
    );
    assert_eq!(removed["removedInstances"], 1);

    // 删干净后文件仍是合法结构
    let v = mcp.call("file_validate", json!({"file": "t.xirang"}));
    assert_eq!(v["count"], 0, "{v}");

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn convert_and_blob_respect_force() {
    let root = tmp_root("conv");
    let mut mcp = Mcp::start(&root);

    // 先建一棵树
    let root_id = mcp.call(
        "node",
        json!({"file": "c.xirang", "action": "create", "name": "词库", "no_history": true}),
    )["created"]
        .as_str()
        .unwrap()
        .to_string();
    mcp.call(
        "node",
        json!({"file": "c.xirang", "action": "create", "parent": root_id,
               "name": "词条", "value": "灯", "no_history": true}),
    );

    // export → 文本
    let out =
        mcp.call("convert", json!({"file": "c.xirang", "action": "export", "format": "json"}));
    let text = out["text"].as_str().unwrap().to_string();
    assert!(text.contains("词库"), "{text}");

    // append：把嵌套 JSON 追成子树
    let app = mcp.call(
        "convert",
        json!({"file": "c.xirang", "action": "append", "data": {"追加": {"字段": "值"}}}),
    );
    assert!(app["nodeCount"].as_u64().unwrap() > 2);

    // import 到非空文件：没有 force 会被拦（避免静默替换整份数据）
    let e = mcp.call_err(
        "convert",
        json!({"file": "c.xirang", "action": "import", "format": "json", "text": text}),
    );
    assert_eq!(e["kind"], "guarded", "{e}");
    // 带 force 才替换
    let imported = mcp.call(
        "convert",
        json!({"file": "c.xirang", "action": "import", "format": "json",
               "text": text, "force": true}),
    );
    assert!(imported["replaced"].as_bool().unwrap());

    // blob：导入 / 查看 / 导出（覆盖要 force）
    std::fs::write(root.join("note.bin"), b"hello blob").unwrap();
    let b = mcp.call(
        "blob",
        json!({"file": "c.xirang", "action": "import", "parent": root_id, "source": "note.bin"}),
    );
    let blob_id = b["imported"].as_str().unwrap().to_string();
    let info = mcp.call("blob", json!({"file": "c.xirang", "action": "info", "node": blob_id}));
    assert_eq!(info["bytes"], 10);
    assert_eq!(info["textPreview"], "hello blob");

    let first = mcp.call(
        "blob",
        json!({"file": "c.xirang", "action": "export", "node": blob_id, "dest": "out.bin"}),
    );
    assert_eq!(first["bytes"], 10);
    assert_eq!(std::fs::read(root.join("out.bin")).unwrap(), b"hello blob");
    let e = mcp.call_err(
        "blob",
        json!({"file": "c.xirang", "action": "export", "node": blob_id, "dest": "out.bin"}),
    );
    assert_eq!(e["kind"], "guarded", "{e}");
    mcp.call(
        "blob",
        json!({"file": "c.xirang", "action": "export", "node": blob_id,
               "dest": "out.bin", "force": true}),
    );

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn query_tree_and_diff_work_over_mcp() {
    let root = tmp_root("query");
    let mut mcp = Mcp::start(&root);

    let root_id = mcp.call(
        "node",
        json!({"file": "q.xirang", "action": "create", "name": "词库", "no_history": true}),
    )["created"]
        .as_str()
        .unwrap()
        .to_string();
    for value in ["灯", "火"] {
        let e = mcp.call(
            "node",
            json!({"file": "q.xirang", "action": "create", "parent": root_id,
                   "name": "词条", "value": value, "no_history": true}),
        );
        let eid = e["created"].as_str().unwrap().to_string();
        mcp.call(
            "node",
            json!({"file": "q.xirang", "action": "create", "parent": eid,
                   "name": "词形", "value": value, "no_history": true}),
        );
    }

    // find
    let hits = mcp.call("query", json!({"file": "q.xirang", "action": "find", "pattern": "火"}));
    assert_eq!(hits.as_array().unwrap().len(), 2, "{hits}");

    // match：按根名 + where 筛值
    let m = mcp.call(
        "query",
        json!({"file": "q.xirang", "action": "match", "root": "词条",
               "where": {"词形": "火"}, "tree": true}),
    );
    let items = m.as_array().unwrap();
    assert_eq!(items.len(), 1, "{m}");
    assert_eq!(items[0]["value"], "火");
    assert_eq!(items[0]["children"][0]["name"], "词形", "tree=true 应带整棵树");

    // shape_of：拓扑相同的子树（两个词条结构一致）
    let first_id = items[0]["id"].as_str().unwrap().to_string();
    let shape = mcp.call(
        "query",
        json!({"file": "q.xirang", "action": "match", "shape_of": first_id}),
    );
    assert_eq!(shape.as_array().unwrap().len(), 2, "同形状的两棵词条都该命中");

    // tree：flat 布局带父边，便于大文件浏览
    let flat = mcp.call("tree", json!({"file": "q.xirang", "layout": "flat", "limit": 3}));
    assert_eq!(flat["nodes"].as_array().unwrap().len(), 3);
    assert_eq!(flat["truncated"], true);
    assert!(flat["nodes"][0]["parent"].is_null() || flat["nodes"][0]["parent"].is_string());

    // tree：默认树形 + 深度限制
    let tree = mcp.call("tree", json!({"file": "q.xirang", "depth": 1}));
    assert_eq!(tree["nodes"].as_array().unwrap().len(), 1, "只展开 1 层 = 只有根");

    // refs / history
    let refs = mcp.call("query", json!({"file": "q.xirang", "action": "refs", "node": root_id}));
    assert!(refs["node"]["id"].is_string());
    let hist =
        mcp.call("query", json!({"file": "q.xirang", "action": "history", "node": root_id}));
    assert_eq!(hist["hasHistory"], false, "no_history 建的节点不该有 @history");

    // diff：同一个文件对比自己 = 无差异
    let d = mcp.call("file_diff", json!({"a": "q.xirang", "b": "q.xirang"}));
    assert_eq!(d["added"].as_array().unwrap().len(), 0);
    assert_eq!(d["changed"].as_array().unwrap().len(), 0);

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn mcp_does_not_touch_the_local_catalog() {
    let root = tmp_root("catalog");
    let (code, _, err) = run({
        let mut c = xr(&root);
        c.args(["new", "k.xirang", "nil", "根", "--no-history"]);
        c
    });
    assert_eq!(code, 0, "{err}");
    // 清掉目录，模拟「纯 MCP 会话」
    std::fs::remove_file(catalog_path(&root)).ok();

    let mut mcp = Mcp::start(&root);
    mcp.call("file_info", json!({"file": "k.xirang"}));
    mcp.call("tree", json!({"file": "k.xirang"}));

    let (_, out, _) = run({
        let mut c = xr(&root);
        c.args(["catalog", "list"]);
        c
    });
    assert!(out.contains("文件: 0 个"), "MCP 不该写本机目录：{out}");

    // 走一次 CLI 读命令，目录里就该有它了（证明前面不是命令本身失效）
    run({
        let mut c = xr(&root);
        c.args(["info", "k.xirang"]);
        c
    });
    let (_, out, _) = run({
        let mut c = xr(&root);
        c.args(["catalog", "list"]);
        c
    });
    assert!(out.contains("文件: 1 个"), "CLI 读命令应登记目录：{out}");

    std::fs::remove_dir_all(&root).ok();
}

/// 批量提交也走 MCP：一次请求改一批（与 CLI 同一条实现：`ops::batch_edit`）。
#[test]
fn node_batch_over_mcp() {
    let root = tmp_root("batch");
    let mut mcp = Mcp::start(&root);

    let root_id = mcp.call(
        "node",
        json!({"file": "b.xirang", "action": "create", "name": "根", "no_history": true}),
    )["created"]
        .as_str()
        .unwrap()
        .to_string();
    let mut ids = Vec::new();
    for i in 0..4 {
        let id = mcp.call(
            "node",
            json!({"file": "b.xirang", "action": "create", "parent": root_id,
                   "name": format!("词{i}"), "value": "旧", "no_history": true}),
        )["created"]
            .as_str()
            .unwrap()
            .to_string();
        ids.push(id);
    }
    let ops: Vec<Value> = ids
        .iter()
        .map(|id| json!({"op": "set", "id": id, "value": "新"}))
        .collect();

    // 预演
    let out = mcp.call(
        "node",
        json!({"file": "b.xirang", "action": "batch", "ops": ops,
               "no_history": true, "dry_run": true}),
    );
    assert_eq!(out["changed"], 4);
    assert_eq!(out["dryRun"], true);

    // 真跑：四条改动（协议声明在之前的 create 时已经补过了，这里不该再来一条）
    let out = mcp.call(
        "node",
        json!({"file": "b.xirang", "action": "batch", "ops": ops, "no_history": true}),
    );
    assert_eq!(out["changed"], 4);
    assert_eq!(out["appended"], 4, "不留痕时每条改动只写一条记录：{out}");
    let (_, tree_out, _) = run({
        let mut c = xr(&root);
        c.args(["tree", "b.xirang", "--no-pager"]);
        c
    });
    assert_eq!(
        tree_out.matches("@protocol = append-v1").count(),
        1,
        "协议声明必须只有一条：{tree_out}"
    );

    // 读得回来（与 CLI 结果一致）
    let (_, out, err) = run({
        let mut c = xr(&root);
        c.args(["find", "b.xirang", "新"]);
        c
    });
    assert!(err.is_empty(), "{err}");
    assert!(out.contains("共 4 个匹配"), "四条都该读到：{out}");

    std::fs::remove_dir_all(&root).ok();
}
