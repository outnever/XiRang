//! 息壤 MCP server（`xr-mcp`）：把共享操作层（`ops`）暴露成结构化工具（JSON-RPC over stdio）。
//!
//! 和 `xr` 共用同一份实现、同一套护栏：能做什么、拦什么，两边一致。
//! 两条入口规则不同，是因为使用者的处境不同：
//! - **路径**：只能操作启动时允许的目录（`--root` / `XIRANG_MCP_ROOTS`，默认进程工作目录）。
//! - **危险操作**：要显式带 `force: true`，否则返回 `guarded` 错误并说明原因。
//! 本机状态（catalog 本机目录、`index`、分片词库目录）不在这里暴露，请用 `xr` CLI。

use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use serde_json::{json, Value};
use xirang_core::codec::Value as XValue;

mod ops;

use ops::{Layout, MatchQuery, OpError, Policy, ViewOpts};

// ============================================================================
// 启动配置
// ============================================================================

struct Config {
    /// 允许操作的目录（至少一个）。
    roots: Vec<PathBuf>,
}

fn usage() {
    println!("息壤 MCP server：xr-mcp");
    println!();
    println!("用法：");
    println!("  xr-mcp [--root <目录>]...");
    println!();
    println!("  --root <目录>   允许操作的目录（可重复）。不指定时用 $XIRANG_MCP_ROOTS，");
    println!("                  再没有就用当前工作目录。相对路径基于第一个 --root 解析，");
    println!("                  越界的路径（含绝对路径）一律拒绝。");
    println!("  走 stdio（JSON-RPC）；把 stdout 接给你的 MCP 客户端。");
}

fn canon_root(raw: &str) -> Result<PathBuf, String> {
    let p = PathBuf::from(raw);
    let c = p.canonicalize().map_err(|e| format!("{raw}：{e}"))?;
    if !c.is_dir() {
        return Err(format!("{raw}：不是目录"));
    }
    Ok(c)
}

fn parse_args() -> Result<Config, String> {
    let mut roots: Vec<PathBuf> = Vec::new();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        match a {
            "--help" | "-h" => {
                usage();
                std::process::exit(0);
            }
            "--version" | "-V" => {
                println!("xr-mcp {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "--root" => {
                let v = args.get(i + 1).ok_or("--root 需要 <目录>")?;
                roots.push(canon_root(v)?);
                i += 2;
            }
            other if other.starts_with("--root=") => {
                roots.push(canon_root(&other["--root=".len()..])?);
                i += 1;
            }
            other => return Err(format!("未知参数：{other}（见 xr-mcp --help）")),
        }
    }
    if roots.is_empty() {
        if let Some(env) = std::env::var_os("XIRANG_MCP_ROOTS") {
            for p in std::env::split_paths(&env) {
                if !p.as_os_str().is_empty() {
                    roots.push(canon_root(&p.to_string_lossy())?);
                }
            }
        }
    }
    if roots.is_empty() {
        // 开箱可用：没配就限定在进程工作目录内，仍然挡住越界路径。
        let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
        roots.push(cwd);
    }
    Ok(Config { roots })
}

// ============================================================================
// 参数读取
// ============================================================================

fn policy(cfg: &Config, args: &Value) -> Policy {
    let force = args.get("force").and_then(|v| v.as_bool()).unwrap_or(false);
    Policy::mcp(force, cfg.roots.clone())
}

fn str_arg(args: &Value, key: &str) -> Result<String, OpError> {
    match args.get(key).and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => Ok(s.to_string()),
        _ => Err(OpError::invalid(format!("缺参数 {key}"))),
    }
}

fn opt_str(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(|v| v.as_str()).filter(|s| !s.is_empty()).map(|s| s.to_string())
}

fn bool_arg(args: &Value, key: &str, default: bool) -> bool {
    args.get(key).and_then(|v| v.as_bool()).unwrap_or(default)
}

fn usz_arg(args: &Value, key: &str) -> Option<usize> {
    args.get(key).and_then(|v| v.as_u64()).map(|n| n as usize)
}

fn action_arg(args: &Value, allowed: &[&str]) -> Result<String, OpError> {
    let a = str_arg(args, "action")?;
    if allowed.contains(&a.as_str()) {
        Ok(a)
    } else {
        Err(OpError::invalid(format!("未知 action：{a}（可用：{}）", allowed.join(" / "))))
    }
}

/// JSON 值 → 字符串（用于 `where` 这类「按值比对」的参数）。
fn value_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// 分片词库目录在 MCP 一律拒绝（本机状态类操作留 CLI）。
fn check_target(pol: &Policy, raw: &str) -> Result<(), OpError> {
    if pol.resolve(raw)?.is_dir() {
        return Err(OpError::unsupported(format!("这是分片词库目录：{raw}")).hint(
            "目录（shard 词库）用 xr CLI 操作（collection / compact）；MCP 只处理单个 .xirang 文件",
        ));
    }
    Ok(())
}

// ============================================================================
// 工具实现（返回 JSON 结果；出错返回 OpError）
// ============================================================================

fn tool_context(cfg: &Config) -> Result<Value, OpError> {
    Ok(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "roots": cfg.roots.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
        "tools": ops::MCP_TOOLS,
        "actions": {
            "query": ops::QUERY_ACTIONS,
            "node": ops::NODE_ACTIONS,
            "template": ops::TEMPLATE_ACTIONS,
            "convert": ops::CONVERT_ACTIONS,
            "blob": ops::BLOB_ACTIONS,
        },
        "notes": [
            "relative paths resolve against roots[0]; anything outside roots is refused",
            "destructive actions need force=true (otherwise you get a guarded error)",
            "this server does not touch the local catalog / sidecar index; use the xr CLI for that",
        ],
    }))
}

fn tool_file_info(cfg: &Config, args: &Value) -> Result<Value, OpError> {
    let file = str_arg(args, "file")?;
    let pol = policy(cfg, args);
    check_target(&pol, &file)?;
    Ok(ops::info(&pol, &ops::NoHooks, &file)?.to_json())
}

fn tool_file_validate(cfg: &Config, args: &Value) -> Result<Value, OpError> {
    let file = str_arg(args, "file")?;
    let pol = policy(cfg, args);
    check_target(&pol, &file)?;
    Ok(ops::validate(&pol, &ops::NoHooks, &file)?.to_json())
}

fn tool_file_diff(cfg: &Config, args: &Value) -> Result<Value, OpError> {
    let a = str_arg(args, "a")?;
    let b = str_arg(args, "b")?;
    let pol = policy(cfg, args);
    check_target(&pol, &a)?;
    check_target(&pol, &b)?;
    Ok(ops::diff(&pol, &ops::NoHooks, &a, &b)?.to_json())
}

fn tool_tree(cfg: &Config, args: &Value) -> Result<Value, OpError> {
    let file = str_arg(args, "file")?;
    let pol = policy(cfg, args);
    check_target(&pol, &file)?;
    let layout = match opt_str(args, "layout").as_deref() {
        None | Some("tree") => Layout::Tree,
        Some("flat") => Layout::Flat,
        Some(other) => {
            return Err(OpError::invalid(format!("未知 layout：{other}（可用：tree / flat）")))
        }
    };
    let opts = ViewOpts {
        skip_aux: bool_arg(args, "skip_aux", false),
        include_ids: bool_arg(args, "include_ids", false),
        max_depth: usz_arg(args, "depth"),
        // 默认限流：大文件不该一次吐给模型（CLI 那边是 --head，默认不限）
        limit: Some(usz_arg(args, "limit").unwrap_or(200)),
    };
    let node = opt_str(args, "node");
    let out = ops::view(&pol, &ops::NoHooks, &file, node.as_deref(), opts, layout, None)?;
    Ok(out.to_json())
}

fn tool_query(cfg: &Config, args: &Value) -> Result<Value, OpError> {
    let file = str_arg(args, "file")?;
    let action = action_arg(args, ops::QUERY_ACTIONS)?;
    let pol = policy(cfg, args);
    check_target(&pol, &file)?;
    match action.as_str() {
        "find" => {
            let pattern = str_arg(args, "pattern")?;
            let hits = ops::find(&pol, &ops::NoHooks, &file, &pattern)?;
            Ok(json!(hits.iter().map(|h| h.to_json()).collect::<Vec<_>>()))
        }
        "match" => {
            let mut wheres: Vec<(String, String)> = Vec::new();
            if let Some(wh) = args.get("where").and_then(|v| v.as_object()) {
                for (k, v) in wh {
                    wheres.push((k.clone(), value_to_string(v)));
                }
            }
            let q = MatchQuery {
                root: opt_str(args, "root"),
                shape_of: opt_str(args, "shape_of"),
                template: opt_str(args, "template"),
                wheres,
                with_tree: bool_arg(args, "tree", false),
            };
            Ok(ops::search(&pol, &ops::NoHooks, &file, &q)?.to_json())
        }
        "instances" => {
            let template = str_arg(args, "template")?;
            Ok(ops::instances(&pol, &ops::NoHooks, &file, &template)?.to_json())
        }
        "refs" => {
            let node = str_arg(args, "node")?;
            Ok(ops::refs(&pol, &ops::NoHooks, &file, &node)?.to_json())
        }
        "history" => {
            let node = str_arg(args, "node")?;
            Ok(ops::history(&pol, &ops::NoHooks, &file, &node)?.to_json())
        }
        _ => unreachable!(),
    }
}

fn tool_node(cfg: &Config, args: &Value) -> Result<Value, OpError> {
    let file = str_arg(args, "file")?;
    let action = action_arg(args, ops::NODE_ACTIONS)?;
    let pol = policy(cfg, args);
    check_target(&pol, &file)?;
    let no_history = bool_arg(args, "no_history", false);
    match action.as_str() {
        "create" => {
            let name = str_arg(args, "name")?;
            let parent = opt_str(args, "parent");
            let value = ops::value_from_json(args.get("value"));
            let o = ops::create_node(
                &pol,
                &ops::NoHooks,
                &file,
                parent.as_deref(),
                &name,
                value,
                no_history,
            )?;
            Ok(json!({
                "created": o.id.to_string(),
                "name": o.name,
                "newShard": o.new_shard,
                "warnings": o.warnings,
            }))
        }
        "set" => {
            let node = str_arg(args, "node")?;
            if args.get("value").is_none() {
                return Err(OpError::invalid("set 需要 value"));
            }
            let value = ops::value_from_json(args.get("value"));
            let o = ops::set_value(&pol, &ops::NoHooks, &file, &node, value, no_history)?;
            Ok(json!({"updated": o.id.to_string()}))
        }
        "rename" => {
            let node = str_arg(args, "node")?;
            let name = str_arg(args, "name")?;
            let o = ops::rename_node(&pol, &ops::NoHooks, &file, &node, &name, no_history)?;
            Ok(json!({"renamed": o.id.to_string(), "name": o.name}))
        }
        "remove" => {
            let node = str_arg(args, "node")?;
            let o = ops::remove_node(&pol, &ops::NoHooks, &file, &node)?;
            Ok(json!({"removed": o.id.to_string(), "emptied": true}))
        }
        "link" => {
            let from = str_arg(args, "from")?;
            let to = str_arg(args, "to")?;
            let o = ops::link_nodes(&pol, &ops::NoHooks, &file, &from, &to, no_history)?;
            Ok(json!({"linked": {"from": o.from.to_string(), "to": o.to.to_string()}}))
        }
        "copy" => {
            let node = str_arg(args, "node")?;
            let parent = opt_str(args, "parent");
            let blank = bool_arg(args, "blank", false);
            let o = ops::copy_node(
                &pol,
                &ops::NoHooks,
                &file,
                &node,
                parent.as_deref(),
                blank,
                no_history,
            )?;
            Ok(json!({
                "copied": o.src_id.to_string(),
                "sourceName": o.src_name,
                "newRoot": o.new_id.to_string(),
                "warnings": o.warnings,
            }))
        }
        "fill" => {
            let root = str_arg(args, "root")?;
            let list = args
                .get("assigns")
                .and_then(|v| v.as_array())
                .ok_or_else(|| OpError::invalid("fill 需要 assigns（[{path, value}] 数组）"))?;
            let mut pairs: Vec<(String, XValue)> = Vec::new();
            for item in list {
                let p = item
                    .get("path")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| OpError::invalid("assigns 每项需要 path"))?;
                pairs.push((p.to_string(), ops::value_from_json(item.get("value"))));
            }
            let o = ops::fill_values(&pol, &ops::NoHooks, &file, &root, &pairs, no_history)?;
            Ok(json!({"assigned": o.count}))
        }
        "revert" => {
            let node = str_arg(args, "node")?;
            let o = ops::revert_node(&pol, &ops::NoHooks, &file, &node)?;
            Ok(json!({
                "reverted": o.id.to_string(),
                "name": o.name,
                "reason": o.no_snapshot,
            }))
        }
        "prune_history" => {
            // 裁剪留痕：丢掉的是回滚能力 → 没有 force 时由 ops 返回 guarded
            let node = str_arg(args, "node")?;
            let keep = usz_arg(args, "keep").unwrap_or(20);
            let before = opt_str(args, "before");
            let o = ops::prune_history(
                &pol,
                &ops::NoHooks,
                &file,
                &node,
                keep,
                before.as_deref(),
                false,
            )?;
            Ok(json!({
                "removed": o.removed,
                "kept": o.kept,
                "nodesBefore": o.nodes_before,
                "nodesAfter": o.nodes_after,
                "bytesBefore": o.bytes_before,
                "bytesAfter": o.bytes_after,
            }))
        }
        _ => unreachable!(),
    }
}

fn tool_template(cfg: &Config, args: &Value) -> Result<Value, OpError> {
    let file = str_arg(args, "file")?;
    let action = action_arg(args, ops::TEMPLATE_ACTIONS)?;
    let pol = policy(cfg, args);
    check_target(&pol, &file)?;
    match action.as_str() {
        "define" => {
            let name = str_arg(args, "name")?;
            let sample = args
                .get("sample")
                .ok_or_else(|| OpError::invalid("define 需要 sample（对象，用来定结构）"))?;
            let o = ops::template_define(&pol, &ops::NoHooks, &file, &name, sample)?;
            Ok(json!({"template": {"id": o.id.to_string(), "name": o.name}}))
        }
        "list" => {
            let items = ops::template_list(&pol, &ops::NoHooks, &file)?;
            Ok(json!({
                "templates": items.iter().map(|t| json!({
                    "id": t.id.to_string(),
                    "name": t.name,
                    "instances": t.instances,
                })).collect::<Vec<_>>()
            }))
        }
        "instantiate" => {
            let template = str_arg(args, "template")?;
            let data = args
                .get("data")
                .ok_or_else(|| OpError::invalid("instantiate 需要 data（记录数组）"))?;
            let records: Vec<Value> = match data {
                Value::Array(a) => a.clone(),
                other => vec![other.clone()],
            };
            let under = opt_str(args, "under");
            let o = ops::import_instances(
                &pol,
                &ops::NoHooks,
                &file,
                &template,
                &records,
                under.as_deref(),
            )?;
            Ok(json!({
                "imported": o.count,
                "template": {"id": o.template_id.to_string(), "name": o.template_name},
            }))
        }
        "remove" => {
            let template = str_arg(args, "template")?;
            let o = ops::template_remove(&pol, &ops::NoHooks, &file, &template)?;
            Ok(json!({
                "removed": o.name,
                "id": o.id.to_string(),
                "removedInstances": o.removed_instances,
            }))
        }
        _ => unreachable!(),
    }
}

fn tool_convert(cfg: &Config, args: &Value) -> Result<Value, OpError> {
    let file = str_arg(args, "file")?;
    let action = action_arg(args, ops::CONVERT_ACTIONS)?;
    let pol = policy(cfg, args);
    check_target(&pol, &file)?;
    match action.as_str() {
        "export" => {
            let format = str_arg(args, "format")?;
            let subtree = opt_str(args, "subtree");
            Ok(ops::export_data(&pol, &ops::NoHooks, &file, &format, subtree.as_deref())?.to_json())
        }
        "import" => {
            let format = str_arg(args, "format")?;
            let text = str_arg(args, "text")?;
            let o = ops::import_data(&pol, &ops::NoHooks, &file, &format, &text)?;
            Ok(json!({
                "nodes": o.nodes,
                "previousNodes": o.previous_nodes,
                "replaced": o.previous_nodes.is_some(),
            }))
        }
        "append" => {
            let data = args
                .get("data")
                .ok_or_else(|| OpError::invalid("append 需要 data（嵌套 JSON）"))?;
            let parent = opt_str(args, "parent");
            let o = ops::import_append(&pol, &ops::NoHooks, &file, parent.as_deref(), data)?;
            Ok(json!({"appended": true, "nodeCount": o.nodes_after}))
        }
        _ => unreachable!(),
    }
}

fn tool_blob(cfg: &Config, args: &Value) -> Result<Value, OpError> {
    let file = str_arg(args, "file")?;
    let action = action_arg(args, ops::BLOB_ACTIONS)?;
    let pol = policy(cfg, args);
    check_target(&pol, &file)?;
    match action.as_str() {
        "import" => {
            let source = str_arg(args, "source")?;
            pol.resolve(&source)?;
            let parent = opt_str(args, "parent");
            let o = ops::blob_import(&pol, &ops::NoHooks, &file, parent.as_deref(), &source)?;
            Ok(json!({"imported": o.id.to_string(), "name": o.name}))
        }
        "export" => {
            let node = str_arg(args, "node")?;
            let dest = str_arg(args, "dest")?;
            pol.resolve(&dest)?;
            let o = ops::blob_export(&pol, &ops::NoHooks, &file, &node, &dest)?;
            Ok(json!({"bytes": o.bytes, "dest": o.dest}))
        }
        "info" => {
            let node = str_arg(args, "node")?;
            Ok(ops::blob_info(&pol, &ops::NoHooks, &file, &node)?.to_json())
        }
        _ => unreachable!(),
    }
}

fn dispatch(cfg: &Config, name: &str, args: &Value) -> Result<Value, OpError> {
    match name {
        "context" => tool_context(cfg),
        "file_info" => tool_file_info(cfg, args),
        "file_validate" => tool_file_validate(cfg, args),
        "file_diff" => tool_file_diff(cfg, args),
        "tree" => tool_tree(cfg, args),
        "query" => tool_query(cfg, args),
        "node" => tool_node(cfg, args),
        "template" => tool_template(cfg, args),
        "convert" => tool_convert(cfg, args),
        "blob" => tool_blob(cfg, args),
        other => Err(OpError::invalid(format!("未知工具：{other}"))),
    }
}

// ============================================================================
// 工具定义（JSON Schema）
// ============================================================================

fn file_prop() -> Value {
    json!({"type": "string", "description": ".xirang 路径（相对第一个 --root，或绝对路径）"})
}

fn force_prop() -> Value {
    json!({"type": "boolean", "description": "危险 / 受保护操作需要显式设为 true（默认 false）"})
}

fn tool_defs() -> Vec<Value> {
    vec![
        json!({
            "name": "context",
            "description": "先看这里：允许操作的目录、版本、各工具支持的动作清单",
            "inputSchema": {"type": "object", "properties": {}}
        }),
        json!({
            "name": "file_info",
            "description": "文件摘要（格式版本 / 节点数 / 根数）",
            "inputSchema": {"type": "object", "properties": {"file": file_prop()}, "required": ["file"]}
        }),
        json!({
            "name": "file_validate",
            "description": "结构校验（E/R 错误码）",
            "inputSchema": {"type": "object", "properties": {"file": file_prop()}, "required": ["file"]}
        }),
        json!({
            "name": "file_diff",
            "description": "对比两个文件（增/删/改，按节点编号）",
            "inputSchema": {"type": "object", "properties": {"a": file_prop(), "b": file_prop()}, "required": ["a", "b"]}
        }),
        json!({
            "name": "tree",
            "description": "树视图。layout=tree 返回嵌套结构；layout=flat 按存放顺序一行一个节点（大文件用 flat + limit）",
            "inputSchema": {"type": "object", "properties": {
                "file": file_prop(),
                "layout": {"type": "string", "enum": ["tree", "flat"], "description": "默认 tree"},
                "node": {"type": "string", "description": "只看这棵子树（节点编号）"},
                "depth": {"type": "integer", "description": "只展开到第 N 层"},
                "limit": {"type": "integer", "description": "最多返回多少个节点，默认 200"},
                "skip_aux": {"type": "boolean", "description": "跳过 @ 开头的辅助节点"},
                "include_ids": {"type": "boolean", "description": "文本视图里是否带编号（对 JSON 结果无影响）"}
            }, "required": ["file"]}
        }),
        json!({
            "name": "query",
            "description": "查询。find=按名字/文本值搜；match=按结构(shape_of)/名字(root)/模板(template) + where 匹配；instances=某模板的实例；refs=引用边；history=@history 快照",
            "inputSchema": {"type": "object", "properties": {
                "file": file_prop(),
                "action": {"type": "string", "enum": ops::QUERY_ACTIONS},
                "pattern": {"type": "string", "description": "find：搜索串"},
                "root": {"type": "string", "description": "match：按节点名锚定"},
                "shape_of": {"type": "string", "description": "match：按该节点的形状码锚定"},
                "template": {"type": "string", "description": "match / instances：模板名或编号"},
                "where": {"type": "object", "description": "match：值约束，如 {\"词形\":\"火\"}"},
                "tree": {"type": "boolean", "description": "match：命中项连整棵树一起返回"},
                "node": {"type": "string", "description": "refs / history：节点编号"}
            }, "required": ["file", "action"]}
        }),
        json!({
            "name": "node",
            "description": "改节点。create=新建；set=改值；rename=改名（编号不变，旧名进 @history）；remove=删除（置空，可 revert 找回）；link=连引用边；copy=复制子树；fill=按名/路径赋值；revert=回滚到最近快照；prune_history=裁剪 @history 留痕（保留最近 keep 条，默认 20；会丢掉回滚能力，需 force）",
            "inputSchema": {"type": "object", "properties": {
                "file": file_prop(),
                "action": {"type": "string", "enum": ops::NODE_ACTIONS},
                "force": force_prop(),
                "no_history": {"type": "boolean", "description": "不写 @history / @created（批量 / 初始数据）"},
                "node": {"type": "string", "description": "set / rename / remove / copy / revert：目标节点编号"},
                "parent": {"type": "string", "description": "create / copy：父节点编号；omit 或 \"nil\" = 自由根"},
                "name": {"type": "string", "description": "create：节点名；rename：新名字（非空，≤255 字节）"},
                "value": {"description": "create / set：新值（按 JSON 类型；字符串即文本）"},
                "from": {"type": "string", "description": "link：起点节点编号"},
                "to": {"type": "string", "description": "link：目标节点编号"},
                "blank": {"type": "boolean", "description": "copy：只复制结构（标量值清空）"},
                "root": {"type": "string", "description": "fill：赋值起点节点编号"},
                "assigns": {"type": "array", "description": "fill：[{path, value}]，path 形如 \"词形\" 或 \"词义/01/释义\"",
                    "items": {"type": "object", "properties": {"path": {"type": "string"}, "value": {}}, "required": ["path"]}},
                "keep": {"type": "integer", "description": "prune_history：保留最近多少条快照，默认 20"},
                "before": {"type": "string", "description": "prune_history：只裁早于该 ISO 时间前缀的快照（如 2026-01-01）"}
            }, "required": ["file", "action"]}
        }),
        json!({
            "name": "template",
            "description": "模板。define=建模板（结构取自 sample）；list=列模板与实例数；instantiate=按模板批量建实例（可 under 挂到某父下）；remove=删模板（连同实例，需 force）",
            "inputSchema": {"type": "object", "properties": {
                "file": file_prop(),
                "action": {"type": "string", "enum": ops::TEMPLATE_ACTIONS},
                "force": force_prop(),
                "name": {"type": "string", "description": "define：模板名"},
                "sample": {"type": "object", "description": "define：结构样例（键→节点名，嵌套→子树）"},
                "template": {"type": "string", "description": "instantiate / remove：模板名或编号"},
                "data": {"description": "instantiate：记录（对象数组，或单个对象）"},
                "under": {"type": "string", "description": "instantiate：把实例挂到这个父节点下（omit = 自由根）"}
            }, "required": ["file", "action"]}
        }),
        json!({
            "name": "convert",
            "description": "格式转换。export=导出为 json/yaml/xml/md 文本；import=用 json/yaml/xml 整文件替换（原文件非空要 force）；append=把嵌套 JSON 追加成子树",
            "inputSchema": {"type": "object", "properties": {
                "file": file_prop(),
                "action": {"type": "string", "enum": ops::CONVERT_ACTIONS},
                "force": force_prop(),
                "format": {"type": "string", "description": "export：json|yaml|xml|md；import：json|yaml|xml"},
                "subtree": {"type": "string", "description": "export：只导出这棵子树"},
                "text": {"type": "string", "description": "import：要导入的内容"},
                "data": {"description": "append：嵌套 JSON（对象→子节点，数组→0,1,2，{\"@ref\":\"名\"}→引用边）"},
                "parent": {"type": "string", "description": "append：挂在哪个父节点下（omit = 自由根）"}
            }, "required": ["file", "action"]}
        }),
        json!({
            "name": "blob",
            "description": "二进制块。import=把文件读进来存成 blob；export=写出为文件（目标已存在要 force）；info=大小 / @format / 文本预览",
            "inputSchema": {"type": "object", "properties": {
                "file": file_prop(),
                "action": {"type": "string", "enum": ops::BLOB_ACTIONS},
                "force": force_prop(),
                "source": {"type": "string", "description": "import：源文件路径（须在允许目录内）"},
                "dest": {"type": "string", "description": "export：目标文件路径（须在允许目录内）"},
                "node": {"type": "string", "description": "export / info：blob 所在节点编号"},
                "parent": {"type": "string", "description": "import：挂到哪个父节点下（omit = 自由根）"}
            }, "required": ["file", "action"]}
        }),
    ]
}

// ============================================================================
// 主循环：JSON-RPC over stdio
// ============================================================================

fn main() {
    let cfg = match parse_args() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("错误：{e}");
            std::process::exit(2);
        }
    };

    // 常驻进程的空闲时间：后台线程按需压实（不阻塞主循环，也不占工具面）
    if xirang_core::wsidx::maintenance_enabled() {
        let roots = cfg.roots.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_secs(10));
            let Some(root) = roots.first() else { continue };
            if xirang_core::wsidx::maintenance_needed(root).is_some() {
                let _ = xirang_core::wsidx::compact(root);
            }
        });
    }

    let stdin = io::stdin();
    let mut stdout = io::stdout();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let method = msg["method"].as_str().unwrap_or("");
        if method.starts_with("notifications/") {
            continue; // 通知不回
        }
        let id = msg.get("id").cloned();
        // JSON-RPC 2.0：方法级错误必须是顶层 `error` 成员（不能塞进 result）；
        // 工具内部的业务错误才走 result.isError。
        let (result, rpc_error): (Option<Value>, Option<Value>) = match method {
            "initialize" => (
                Some(json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": { "tools": {} },
                    "serverInfo": {
                        "name": "xirang",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                })),
                None,
            ),
            "ping" => (Some(json!({})), None),
            "tools/list" => (Some(json!({ "tools": tool_defs() })), None),
            "tools/call" => {
                let name = msg["params"]["name"].as_str().unwrap_or("");
                let args = msg["params"]["arguments"].clone();
                let v = match dispatch(&cfg, name, &args) {
                    Ok(content) => json!({
                        "content": [{ "type": "text", "text": content.to_string() }]
                    }),
                    Err(e) => json!({
                        "isError": true,
                        "content": [{
                            "type": "text",
                            "text": json!({"error": e.to_json()}).to_string()
                        }]
                    }),
                };
                (Some(v), None)
            }
            _ => (None, Some(json!({ "code": -32601, "message": format!("未知方法：{method}") }))),
        };
        if let Some(id) = id {
            let resp = match rpc_error {
                Some(e) => json!({ "jsonrpc": "2.0", "id": id, "error": e }),
                None => json!({ "jsonrpc": "2.0", "id": id, "result": result.unwrap_or(Value::Null) }),
            };
            writeln!(stdout, "{}", resp).ok();
        }
    }
}
