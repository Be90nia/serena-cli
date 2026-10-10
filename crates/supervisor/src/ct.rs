//! recipe 批2 CT 低依赖四件（local/recipe-plan.md §2 批2）：
//!
//! - `ct_tldr(file)`：overview 摘要 + symbol-tree top3 + find-test + 首 10 行。
//! - `ct_verify(file, txn)`：diagnostics + format-check（broken 列表已裁决移除，
//!   计划缺陷8，留 ai-hint 票）；`txn` 可选带出被验证写事务的上下文（批4
//!   fix-bug 前后对照用）。
//! - `ct_impact(sym)`：refs + type-analysis（定义处 hover）+ find-test。
//! - `ct_recent_activity(n)`：last-edited（undo store 活跃事务 manifest 派生；
//!   缺陷7 已裁决砍 last-read）+ git log porcelain。
//!
//! 全部内部 fn，零 CLI/wire 面（计划 §0/§1.1：私有模块，不加 execute_tool
//! 分支、不进 catalog）。预算机制同批1（§1.2/§3）：各件自限 `*_BUDGET_TOKENS`
//! 常量（4 bytes ≈ 1 token），超限按节裁剪——字符串节缩整行、list 节缩条，
//! 写 `truncated` + `original_count`（§10-G 词汇）。
//!
//! ponytail: 不做树形精确裁剪——recipe 编排层（批4）只消费各节就绪判定，
//! 粗粒度足够。
//!
//! 批3 追加（同文件，local/recipe-plan.md §2 批3）：
//!
//! - `ct_goto_callers(sym)`：定义定位 → find_referencing_symbols（caller 聚类）
//!   + find_referencing_code_snippets（调用点上下文）。
//! - `ct_smart_edit(file, sym, transform, new_name)`：CT-3 两义定死 {rename,
//!   extract}——rename 委托既有 LSP rename；extract 仅整符号抽取（fn 提为同级
//!   新 fn，原位替换调用）。LS/解析能力不覆盖 → 确定性 BadArgs（detail 以
//!   "language server does not support" 开头），不降级硬造。
//! - `ct_pick(query)`：模糊候选（整 query + 逐 token 召回合并打分），AI 1 token 选。
//! - `ct_write_with_tests(file, code, test_file, tests)`：事务内只含写步（两文件），
//!   测试执行在事务外（计划 §1.7）；报告带 txn_id + run_test 信封。
//! - `ct_define_feature(name[, file])`：找已有 + caller 检索 + find-test；新名写
//!   stub（仅 .rs 目标，确定性拒绝其他扩展名）。
//! - `ct_review_diff([txn_id])`：diff + 逐文件关联 test 检测 + 跑测 + 报告。

// CT 零 wire 面（计划 §1.1 私有模块）：批3/批4 recipe 编排接线之前，无生产
// 调用方（唯一消费者是本模块测试）——模块级 dead_code 豁免，接线后删除。
#![allow(dead_code)]

use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};

use crate::ToolError;

/// `ct_tldr` 预算（tokens，计划 §3 数值表）。
pub(crate) const TLDR_BUDGET_TOKENS: usize = 800;
/// `ct_verify` 预算。
pub(crate) const VERIFY_BUDGET_TOKENS: usize = 600;
/// `ct_impact` 预算。
pub(crate) const IMPACT_BUDGET_TOKENS: usize = 500;
/// `ct_recent_activity` 预算。
pub(crate) const RECENT_BUDGET_TOKENS: usize = 300;

/// token 预算 → 字节（4 bytes ≈ 1 token，批1 同近似）。
fn budget_bytes(tokens: usize) -> usize {
    tokens * 4
}

/// ct_tldr 首行数。
const HEAD_LINES: usize = 10;
/// ct_tldr symbol-tree 取前 N 个（documentSymbol 文档序）。
const SYMBOL_TOP_N: usize = 3;
/// type-analysis hover 内字符串的字符上限（RA hover 带长文档，预算卫生）。
const HOVER_MAX_CHARS: usize = 200;
/// git log 子进程超时；超时/失败显式记 `git_log_skipped`，不静默。
const GIT_TIMEOUT_SECS: u64 = 10;
/// recent-activity 单列表条数上限（预算第二道防线在 truncate）。
const RECENT_MAX_ENTRIES: usize = 20;

// ============ ct_tldr：overview + symbol-tree top3 + find-test + 首行 ============

/// `ct_tldr <file>`：单文件速览。find-test 目标 = 文件首个具名符号
/// （documentSymbol 文档序），无符号回落文件名主干（tests/ 镜像探针按主干命中）。
pub(crate) async fn ct_tldr(
    sup: &crate::Supervisor,
    root: &Path,
    file: &str,
) -> Result<Value, ToolError> {
    if file.is_empty() {
        return Err(ToolError::BadArgs {
            detail: "file must not be empty".into(),
        });
    }
    let hits = sup.tool_overview(root, file, None).await?;
    let symbols_top: Vec<Value> = hits
        .iter()
        .take(SYMBOL_TOP_N)
        .map(|h| {
            json!({
                "name": h.name,
                "kind": h.kind,
                "line": h.range.start.line + 1,
            })
        })
        .collect();
    let symbol = hits
        .iter()
        .map(|h| h.name.as_str())
        .find(|n| !n.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| file_stem(file));
    let find_test = crate::recipe::find_test(sup, root, &symbol).await?;
    let head = first_lines(root, file, HEAD_LINES).await?;
    let mut env = json!({
        "file": file,
        "symbol_count": hits.len(),
        "symbols_top": symbols_top,
        "find_test_symbol": symbol,
        "find_test": find_test,
        "head": head,
    });
    fit_tldr(&mut env);
    Ok(env)
}

/// ct_tldr 信封预算内收缩：head 缩行（计划名序最末 = 语义优先级最低）→
/// find_test.hits 逐条回退 → symbols_top 截条。三段之后骨架（file/
/// symbol_count/≤3 紧凑符号 + find_test 壳）远小于预算。
fn fit_tldr(env: &mut Value) {
    let budget = budget_bytes(TLDR_BUDGET_TOKENS);
    if env_len(env) > budget {
        shrink_lines_field(env, "head", budget);
    }
    if env_len(env) > budget {
        shrink_nested_hits(env, budget);
    }
    if env_len(env) > budget {
        crate::recipe::truncate_list_field(env, "symbols_top", budget);
    }
}

/// find_test.hits 按整信封口径逐条回退到预算内（recipe::truncate_list_field
/// 按子信封度量，嵌套在多节信封里会漏算外层骨架）。回退 → find_test 内写
/// truncated:true + original_count（原条数，§10-G 词汇）。
fn shrink_nested_hits(env: &mut Value, budget: usize) {
    let original = env
        .get("find_test")
        .and_then(|ft| ft.get("hits"))
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    let mut popped = 0usize;
    // +48：循环收敛后补写的 truncated/original_count 旗标开销，否则旗标会把
    // 已达标的信封顶回预算外。
    while env_len(env) + 48 > budget {
        let hits = env
            .get_mut("find_test")
            .and_then(|ft| ft.get_mut("hits"))
            .and_then(Value::as_array_mut);
        let Some(hits) = hits else { break };
        if hits.pop().is_none() {
            break;
        }
        popped += 1;
    }
    if popped > 0
        && let Some(ft) = env.get_mut("find_test")
    {
        ft["truncated"] = json!(true);
        ft["original_count"] = json!(original);
    }
}

/// 文件首 n 行（读失败 → BadArgs：tldr 目标必须存在）。
async fn first_lines(root: &Path, file: &str, n: usize) -> Result<String, ToolError> {
    let body = tokio::fs::read_to_string(root.join(file))
        .await
        .map_err(|e| ToolError::BadArgs {
            detail: format!("cannot read {file}: {e}"),
        })?;
    Ok(body.lines().take(n).collect::<Vec<_>>().join("\n"))
}

/// 文件路径主干（正反斜杠通吃）；无扩展名返回原名。
fn file_stem(file: &str) -> String {
    let name = file.rsplit(['/', '\\']).next().unwrap_or(file);
    name.split_once('.')
        .map(|(s, _)| s.to_string())
        .unwrap_or_else(|| name.to_string())
}

// ============ ct_verify：diagnostics + format-check ============

/// `ct_verify <file> [txn]`：诊断清单 + format-check（`textDocument/formatting`
/// 返回的编辑集非空 = 会改动 → `format_ok:false`，不落盘）。`txn` 给定时附带
/// 该 undo 事务上下文与 file_touched 判定（批4 fix-bug 前后对照消费）。
pub(crate) async fn ct_verify(
    sup: &crate::Supervisor,
    root: &Path,
    file: &str,
    txn: Option<u64>,
) -> Result<Value, ToolError> {
    if file.is_empty() {
        return Err(ToolError::BadArgs {
            detail: "file must not be empty".into(),
        });
    }
    let diag = sup.tool_diagnostics(root, file, None, None).await?;
    // format-check best-effort：诊断是主信号。LS 后端缺格式化器（如 toolchain
    // 未装 rustfmt）→ 显式记 format_skipped，format_ok 缺席 = 未知，不冒充布尔
    // 也不拖垮诊断段。
    let (format_ok, format_edits, format_skipped) = match sup.tool_format(root, file, None, None, None).await {
        Ok(edits) => (Some(edits.is_empty()), edits.len(), None),
        // bd P2-9a：-32601 = LS 无 formatting 能力——原始 rpc jargon（"core error:
        // rpc error -32601: Unhandled method ..."）换人话一行，写 recipe 每步可见。
        Err(ToolError::Core(crate::CoreErrorWire::Rpc { code: -32601, .. })) => (
            None,
            0,
            Some("format skipped: LS does not support formatting".to_string()),
        ),
        Err(e) => (None, 0, Some(format!("format-check skipped: {e}"))),
    };
    let mut env = json!({
        "file": file,
        "diagnostics": diag.get("items").cloned().unwrap_or_else(|| json!([])),
        // tool_diagnostics 语义：pending:true 时 items 空不代表无错（快照可能陈旧）。
        "pending": diag.get("pending").and_then(Value::as_bool).unwrap_or(false),
        "format_ok": format_ok,
        "format_edits": format_edits,
    });
    if let Some(note) = format_skipped {
        env["format_skipped"] = json!(note);
    }
    if let Some(id) = txn {
        let snap = crate::undo::read_txn(root, Some(id)).await?;
        let want = file.replace('\\', "/");
        env["txn"] = json!({
            "txn_id": snap.txn_id,
            "timestamp": snap.timestamp,
            "file_touched": snap
                .files
                .iter()
                .any(|f| crate::recipe::rel_forward(root, &f.path) == want),
        });
    }
    crate::recipe::truncate_list_field(&mut env, "diagnostics", budget_bytes(VERIFY_BUDGET_TOKENS));
    Ok(env)
}

// ============ ct_impact：refs + type-analysis + find-test ============

/// `ct_impact <sym>`：符号定义定位 → 全量 refs → 定义处 hover（type-analysis）
/// → find-test。refs 是核心步，失败即停；hover 辅助步失败显式记 skipped。
pub(crate) async fn ct_impact(
    sup: &crate::Supervisor,
    root: &Path,
    symbol: &str,
) -> Result<Value, ToolError> {
    if symbol.is_empty() {
        return Err(ToolError::BadArgs {
            detail: "symbol must not be empty".into(),
        });
    }
    let (items, _, _) = sup.tool_find_symbol(root, symbol, 20, None).await?;
    let Some(hit) = items.iter().find(|i| i.name == symbol) else {
        return Err(ToolError::BadArgs {
            detail: format!("symbol `{symbol}` not found by workspace/symbol"),
        });
    };
    let Some(path) = crate::uri_to_path(&hit.uri) else {
        return Err(ToolError::BadArgs {
            detail: format!("symbol `{symbol}` has non-file uri: {}", hit.uri),
        });
    };
    let def_rel = crate::recipe::rel_forward(root, &path.to_string_lossy());
    // SymbolHit.range 与 tool_refs/tool_hover 同为 LSP 0-based 原样透传（A3b #2：
    // 此前多 +1 按 1-based 传 → refs/hover 请求错位一行一列）。
    let (line, col) = (hit.range.start.line, hit.range.start.character);
    let refs = sup.tool_refs(root, &def_rel, line, col, None).await?;
    let refs_count = refs.len();
    let ref_entries: Vec<Value> = refs.iter().map(|loc| ref_entry(root, loc)).collect();
    let type_hover = match sup.tool_hover(root, &def_rel, line, col, None).await {
        Ok(Some(h)) => {
            let mut v = serde_json::to_value(&h).unwrap_or(Value::Null);
            clamp_json_strings(&mut v, HOVER_MAX_CHARS);
            v
        }
        Ok(None) => Value::Null,
        Err(e) => json!({ "skipped": format!("hover failed: {e}") }),
    };
    let find_test = crate::recipe::find_test(sup, root, symbol).await?;
    let mut env = json!({
        "symbol": symbol,
        "definition": { "file": def_rel, "line": line, "col": col },
        "refs_count": refs_count,
        "refs": ref_entries,
        "type_hover": type_hover,
        "find_test": find_test,
    });
    let budget = budget_bytes(IMPACT_BUDGET_TOKENS);
    crate::recipe::truncate_list_field(&mut env, "refs", budget);
    if env_len(&env) > budget {
        shrink_nested_hits(&mut env, budget);
    }
    Ok(env)
}

/// Location → 紧凑引用条目（绝对 uri → 相对正斜杠路径，1-based 行列；
/// 非 file uri 保底原样带出，不静默丢）。
fn ref_entry(root: &Path, loc: &lsp_types::Location) -> Value {
    let file = crate::uri_to_path(loc.uri.as_str())
        .map(|p| crate::recipe::rel_forward(root, &p.to_string_lossy()))
        .unwrap_or_else(|| loc.uri.as_str().to_string());
    json!({
        "file": file,
        "line": loc.range.start.line + 1,
        "col": loc.range.start.character + 1,
    })
}

/// 递归 clamp Value 内所有字符串到 max chars（超长 hover 文档类字段的预算卫生）。
fn clamp_json_strings(v: &mut Value, max_chars: usize) {
    match v {
        Value::String(s) => {
            if s.chars().count() > max_chars {
                let mut clamped: String = s.chars().take(max_chars).collect();
                clamped.push('…');
                *s = clamped;
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|i| clamp_json_strings(i, max_chars)),
        Value::Object(map) => map.values_mut().for_each(|x| clamp_json_strings(x, max_chars)),
        _ => {}
    }
}

// ============ ct_recent_activity：last-edited（undo manifest）+ git log ============

/// `ct_recent_activity <n>`：undo store 活跃事务派生 last-edited + git log
/// porcelain 近期提交。undone-* 已回滚不计编辑。
pub(crate) async fn ct_recent_activity(root: &Path, n: usize) -> Result<Value, ToolError> {
    let store = crate::undo::store_for(root)?;
    ct_recent_activity_at(&store, root, n).await
}

/// [`ct_recent_activity`] 的存储路径注入版（单测用，undo::read_txn_at 同款）。
pub(crate) async fn ct_recent_activity_at(
    store: &Path,
    git_root: &Path,
    n: usize,
) -> Result<Value, ToolError> {
    let n = n.min(RECENT_MAX_ENTRIES);
    let (last_edited, manifest_skipped) = last_edited_from_store(store, git_root, n);
    let (git_log, git_note) = git_log_recent(git_root, n).await;
    let mut env = json!({ "last_edited": last_edited, "git_log": git_log });
    if manifest_skipped > 0 {
        env["manifest_skipped"] = json!(manifest_skipped);
    }
    if let Some(note) = git_note {
        env["git_log_skipped"] = json!(note);
    }
    let budget = budget_bytes(RECENT_BUDGET_TOKENS);
    crate::recipe::truncate_list_field(&mut env, "last_edited", budget);
    crate::recipe::truncate_list_field(&mut env, "git_log", budget);
    Ok(env)
}

/// undo store 活跃事务（txn-*）manifest 派生 last-edited：按 (timestamp,
/// txn_id) 降序、路径去重留最新、取前 n 条。store 未建 = 尚无编辑，合法空；
/// 单个 manifest 损坏跳过并计数（聚合视图不因单目录毒化，数量显式带出）。
fn last_edited_from_store(store: &Path, root: &Path, n: usize) -> (Vec<Value>, usize) {
    let mut recs: Vec<(u64, u64, String)> = Vec::new();
    let mut skipped = 0usize;
    let Ok(rd) = std::fs::read_dir(store) else {
        return (Vec::new(), 0);
    };
    for ent in rd.flatten() {
        let Some(name) = ent.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some(Ok(dir_n)) = name.strip_prefix("txn-").map(|s| s.parse::<u64>()) else {
            continue;
        };
        let Ok(body) = std::fs::read_to_string(store.join(&name).join("manifest.json")) else {
            skipped += 1;
            continue;
        };
        let Ok(v) = serde_json::from_str::<Value>(&body) else {
            skipped += 1;
            continue;
        };
        let ts = v.get("timestamp").and_then(Value::as_u64).unwrap_or(0);
        let txn_id = v.get("txn_id").and_then(Value::as_u64).unwrap_or(dir_n);
        for f in v
            .get("files")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(p) = f.get("path").and_then(Value::as_str) {
                recs.push((ts, txn_id, p.to_string()));
            }
        }
    }
    recs.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for (ts, txn_id, p) in recs {
        if out.len() >= n {
            break;
        }
        let rel = crate::recipe::rel_forward(root, &p);
        if !seen.insert(rel.clone()) {
            continue;
        }
        out.push(json!({ "path": rel, "txn_id": txn_id, "timestamp": ts }));
    }
    (out, skipped)
}

/// `git log -n N --name-status --pretty=format:%x01<H>\t<epoch>\t<s>`：
/// \x01 作记录头分隔（subject 可含任意字符，安全切分）。git 缺失 / 非 repo /
/// 超时 → (空, Some(原因))，不静默。
async fn git_log_recent(root: &Path, n: usize) -> (Vec<Value>, Option<String>) {
    if n == 0 {
        return (Vec::new(), None);
    }
    let out = tokio::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("log")
        .arg(format!("-n {n}"))
        .arg("--name-status")
        .arg("--pretty=format:%x01%H%x09%ct%x09%s")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .output();
    let res = match tokio::time::timeout(Duration::from_secs(GIT_TIMEOUT_SECS), out).await {
        Err(_) => {
            return (
                Vec::new(),
                Some(format!("git log timed out after {GIT_TIMEOUT_SECS}s")),
            )
        }
        Ok(res) => res,
    };
    let out = match res {
        Err(e) => return (Vec::new(), Some(format!("git log spawn failed: {e}"))),
        Ok(o) if !o.status.success() => {
            let err = String::from_utf8_lossy(&o.stderr);
            return (Vec::new(), Some(format!("git log failed: {}", err.trim())));
        }
        Ok(o) => o,
    };
    (
        parse_git_log_porcelain(&String::from_utf8_lossy(&out.stdout)),
        None,
    )
}

/// porcelain 解析（纯函数）：每记录首行 `sha\tepoch\tsubject`，其后非空行 =
/// `STATUS\tpath[\tpath2]`（rename/copy 双路径）。畸形行跳过，epoch 解析失败
/// 容忍为 0。
fn parse_git_log_porcelain(text: &str) -> Vec<Value> {
    let mut out = Vec::new();
    for rec in text.split('\x01') {
        let mut lines = rec.lines();
        let Some(head) = lines.next() else {
            continue;
        };
        let mut parts = head.splitn(3, '\t');
        let (Some(sha), Some(epoch), subject) =
            (parts.next(), parts.next(), parts.next().unwrap_or_default())
        else {
            continue;
        };
        if sha.is_empty() {
            continue;
        }
        let mut files = Vec::new();
        for line in lines {
            if line.trim().is_empty() {
                continue;
            }
            let mut f = line.splitn(3, '\t');
            let status = f.next().unwrap_or_default();
            let path = f.next().unwrap_or_default();
            if path.is_empty() {
                continue;
            }
            let mut e = json!({ "status": status, "path": path });
            if let Some(p2) = f.next() {
                e["path2"] = json!(p2);
            }
            files.push(e);
        }
        out.push(json!({
            "sha": sha,
            "epoch": epoch.parse::<u64>().unwrap_or(0),
            "subject": subject,
            "files": files,
        }));
    }
    out
}

// ============ 批3：CT 深水六件（local/recipe-plan.md §2 批3） ============

/// `ct_goto_callers` 预算（tokens，计划 §3 数值表）。
pub(crate) const GOTO_CALLERS_BUDGET_TOKENS: usize = 600;
/// `ct_smart_edit` 预算。
pub(crate) const SMART_EDIT_BUDGET_TOKENS: usize = 400;
/// `ct_pick` 预算。
pub(crate) const PICK_BUDGET_TOKENS: usize = 300;
/// `ct_write_with_tests` 预算。
pub(crate) const WRITE_TESTS_BUDGET_TOKENS: usize = 800;
/// `ct_define_feature` 预算。
pub(crate) const DEFINE_FEATURE_BUDGET_TOKENS: usize = 600;
/// `ct_review_diff` 预算。
pub(crate) const REVIEW_DIFF_BUDGET_TOKENS: usize = 1000;

/// goto-callers / define-feature refs 与 snippets 的条数上限（预算第二道防线在 truncate）。
const CALLER_MAX_REFS: usize = 40;
/// goto-callers 调用点上下文行数（前后各 N 行由 ref_tools 内取）。
const SNIPPET_CONTEXT_LINES: u32 = 2;
/// pick 返回候选上限（AI 1 token 选择的列表长度卫生）。
const PICK_MAX_CANDIDATES: usize = 10;
/// pick 单 query 的 workspace/symbol limit。
const PICK_QUERY_LIMIT: usize = 20;
/// review-diff 关联 test 检测的最大 diff 文件数（预算卫生）。
const REVIEW_DIFF_MAX_FILES: usize = 5;
/// ct_txn 收口写门 who 标签。
const CT_COMMIT_GATE: &str = "ct-commit";

/// 符号定义定位结果（workspace/symbol 精确名命中）。
pub(crate) struct DefHit {
    /// 相对 root 的正斜杠路径。
    pub(crate) file: String,
    /// documentSymbol range.start（0-based；可能是文档注释行）。
    pub(crate) line0: u32,
    pub(crate) col0: u32,
}

/// CT-3 确定性能力错误：LS/文本解析能力不覆盖该操作（码恒 BadArgs，detail
/// 以 "language server does not support" 开头；计划 §2 批3 CT-3 规格）。
fn unsupported(op: &str, why: String) -> ToolError {
    ToolError::BadArgs {
        detail: format!("language server does not support {op}: {why}"),
    }
}

/// workspace/symbol 精确名定位（ct_goto_callers / ct_define_feature 共用，
/// ct_impact 同款两步法）。
pub(crate) async fn def_hit(
    sup: &crate::Supervisor,
    root: &Path,
    symbol: &str,
) -> Result<DefHit, ToolError> {
    let (items, _, _) = sup.tool_find_symbol(root, symbol, 20, None).await?;
    let Some(hit) = items.iter().find(|i| i.name == symbol) else {
        return Err(ToolError::BadArgs {
            detail: format!("symbol `{symbol}` not found by workspace/symbol"),
        });
    };
    let Some(path) = crate::uri_to_path(&hit.uri) else {
        return Err(ToolError::BadArgs {
            detail: format!("symbol `{symbol}` has non-file uri: {}", hit.uri),
        });
    };
    // LS uri 的盘符/段大小写可与 root 漂移（RA 对 TEMP 类目录实测发过小写盘符
    // 形态）——canonicalize 拿盘上真实形态再 rel_forward，否则 strip_prefix 失配
    // 会把 def.file 原样吐成绝对路径，下游 guarded_join 一律拒写。
    let path = dunce::canonicalize(&path).unwrap_or(path);
    Ok(DefHit {
        file: crate::recipe::rel_forward(root, &path.to_string_lossy()),
        line0: hit.range.start.line,
        col0: hit.range.start.character,
    })
}

/// 名字 token 的 1-based 行列：item 文本内定位（跳过注释/属性行），叠加上
/// documentSymbol range.start。item 取不到/名字未命中 → 回退 range.start
/// （ct_impact 同款）。refs/rename 请求必须落在名字上——落注释行语义层静默空。
/// ponytail: col 按字符计，与 LSP UTF-16 仅在名字前出现非 BMP 字符时有差。
pub(crate) async fn name_position(
    sup: &crate::Supervisor,
    root: &Path,
    symbol: &str,
    def: &DefHit,
) -> (u32, u32) {
    let item = sup
        .tool_symbol_body(root, &def.file, symbol, None)
        .await
        .ok();
    match item.as_deref().and_then(|t| locate_signature_name(t, symbol)) {
        Some((_, line_off, col)) => (
            def.line0 + line_off as u32 + 1,
            if line_off == 0 {
                def.col0 + col as u32 + 1
            } else {
                col as u32 + 1
            },
        ),
        None => (def.line0 + 1, def.col0 + 1),
    }
}

/// fn item 内签名名字 token：`(字节偏移, 行偏移, 行内列)`（均 0-based）。
/// 注释（`//`）与属性行（`#`）跳过；名字前的非空白必须是 `fn` 关键字
/// （pub const fn / extern "C" fn 覆盖）；取首个命中（签名先于体内嵌套同名
/// fn）。未命中 → None（非 fn item：struct/impl/const 等）。
fn locate_signature_name(item: &str, symbol: &str) -> Option<(usize, usize, usize)> {
    let is_word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let bytes = item.as_bytes();
    let mut search = 0usize;
    while let Some(rel) = item[search..].find(symbol) {
        let off = search + rel;
        let end = off + symbol.len();
        let before_ok = off == 0 || !is_word(bytes[off - 1]);
        let after_ok = end == bytes.len() || !is_word(bytes[end]);
        let line_start = item[..off].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let line_head = item[line_start..off].trim_start();
        let keyword = item[..off].trim_end().ends_with("fn");
        if before_ok && after_ok && keyword && !line_head.starts_with("//") && !line_head.starts_with('#')
        {
            let line_off = item[..off].matches('\n').count();
            return Some((off, line_off, off - line_start));
        }
        search = end;
    }
    None
}

/// 签名参数串与 body 开 `{` 的字节偏移：`(params, body_open)`。名字后无 `(`
/// （struct/impl）或 params 闭合后无 `{` 而先遇 `;`（trait 声明 `fn f();`）→ None。
fn split_fn_params_body(item: &str, name_off: usize) -> Option<(String, usize)> {
    let open = item[name_off..].find('(')? + name_off;
    let bytes = item.as_bytes();
    let mut depth = 0i32;
    let mut close = None;
    for (i, &b) in bytes.iter().enumerate().skip(open) {
        match b {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(i);
                    break;
                }
            }
            _ => {}
        }
    }
    let close = close?;
    for (j, &b) in bytes.iter().enumerate().skip(close + 1) {
        match b {
            b'{' => return Some((item[open + 1..close].to_string(), j)),
            b';' => return None,
            _ => {}
        }
    }
    None
}

/// 参数绑定量：顶层逗号切分，每参取首个深度 0 `:` 前的绑定（剥 `&mut `/`&`/
/// `mut ` 前缀）。`self` 形参归一为 `self`。解构形参（`(a, b): T`）、`_` 等
/// 调用侧无法重建的形状 → None（确定性错误，不硬造）。
/// ponytail: `<`/`>` 深度计数忽略 `->` 的 `>`（前一字符 '-'）——泛型闭包边界
/// 病态形状靠 None 拒绝兜底。
fn param_bindings(params: &str) -> Option<Vec<String>> {
    let bytes = params.as_bytes();
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'(' | b'[' | b'<' => depth += 1,
            b')' | b']' => depth -= 1,
            b'>' if depth > 0 && i > 0 && bytes[i - 1] != b'-' => depth -= 1,
            b',' if depth == 0 => {
                push_binding(&mut out, &params[start..i])?;
                start = i + 1;
            }
            _ => {}
        }
    }
    push_binding(&mut out, &params[start..])?;
    Some(out)
}

/// 单段绑定解析；空段（空参数串 / 尾逗号）跳过。
fn push_binding(out: &mut Vec<String>, param: &str) -> Option<()> {
    if param.trim().is_empty() {
        return Some(());
    }
    out.push(binding_name(param)?);
    Some(())
}

/// 单个形参的绑定名（`x: i32` → `x`；`&mut self` → `self`）。
fn binding_name(param: &str) -> Option<String> {
    let head = param.split(':').next()?.trim();
    let stripped = head
        .strip_prefix("&mut ")
        .or_else(|| head.strip_prefix('&'))
        .unwrap_or(head)
        .trim();
    let stripped = stripped.strip_prefix("mut ").unwrap_or(stripped).trim();
    if stripped.is_empty()
        || stripped == "_"
        || stripped
            .chars()
            .any(|c| !(c.is_ascii_alphanumeric() || c == '_'))
    {
        return None;
    }
    Some(stripped.to_string())
}

/// extract 的原位调用表达式：self 方法 → `self.<new>(args)`（receiver 不重复
/// 入参）；自由/关联 fn → `<new>(args)`（关联 fn 词法作用域内裸名可达）。
fn build_call_expr(new_name: &str, bindings: &[String]) -> String {
    if bindings.first().is_some_and(|b| b == "self") {
        format!("self.{}({})", new_name, bindings[1..].join(", "))
    } else {
        format!("{}({})", new_name, bindings.join(", "))
    }
}

/// body 首个非空行的缩进（保持原文件风格）；body 无内容行 → 4 空格。
fn body_indent(item: &str, body_open: usize) -> String {
    for l in item[body_open + 1..].lines() {
        if l.trim().is_empty() {
            continue;
        }
        let ws = l.len() - l.trim_start().len();
        return l[..ws].to_string();
    }
    "    ".to_string()
}

/// 新符号 rust stub（define-feature 落盘文本；todo! 确定性占位）。
fn rust_stub(name: &str) -> String {
    format!("pub fn {name}() {{\n    todo!(\"implement {name}\")\n}}\n")
}

/// CT 写步统一事务边界：fresh uid + TXN_UID scope 内执行写步（各写工具内的
/// recorded_write 自动聚合记账），成功 → 写门内 commit（execute_tool 收口同款）
/// 并读回 txn_id；失败 → 快照丢弃、原错误透传。
/// 前置：闭包至少完成一笔 recorded_write（CT 写步路径恒满足），否则读回的是
/// 更早事务的 id。
pub(crate) async fn ct_txn<T, F>(root: &Path, f: F) -> Result<(T, u64), ToolError>
where
    F: Future<Output = Result<T, ToolError>>,
{
    let uid = crate::undo::next_uid();
    let guard = crate::undo::TxnGuard::new(uid);
    // bd serena-rust-15jb：写步内 recorded_write 走 WAL 先记账后写盘，需 store
    // 可见（与 execute_tool 写类路径同款接线）。
    let store = crate::undo::store_for(root)?;
    let result = crate::undo::TXN_STORE
        .scope(store, crate::undo::TXN_UID.scope(uid, f))
        .await;
    match result {
        Ok(v) => {
            // commit 落盘进写门（与 undo/redo 恢复路径的 prune/rename 互斥）。
            let _gate = crate::write_gate::acquire(CT_COMMIT_GATE).await?;
            let committed = crate::undo::commit(root, uid).await;
            guard.settle();
            committed?;
            let snap = crate::undo::read_txn(root, None).await?;
            Ok((v, snap.txn_id))
        }
        Err(e) => {
            crate::undo::abort(uid);
            guard.settle();
            Err(e)
        }
    }
}

/// `ct_goto_callers <sym>`：定义定位 → find_referencing_symbols（按外层符号
/// 聚类 = caller 集合）→ find_referencing_code_snippets（调用点单行上下文，
/// 辅助步失败显式记 skipped，不拖垮主段）。
pub(crate) async fn ct_goto_callers(
    sup: &crate::Supervisor,
    root: &Path,
    symbol: &str,
) -> Result<Value, ToolError> {
    if symbol.is_empty() {
        return Err(ToolError::BadArgs {
            detail: "symbol must not be empty".into(),
        });
    }
    let def = def_hit(sup, root, symbol).await?;
    // workspace/symbol 的 location 即标识符所在位置（lib.rs find_symbol_node 同款
    // 惯例），直传即可；信封定义位换 1-based。
    let (refs, _aap4) = sup
        .tool_referencing_symbols(root, &def.file, def.line0, def.col0, None)
        .await?;
    let refs: Vec<_> = refs.into_iter().take(CALLER_MAX_REFS).collect();
    let (line, col) = (def.line0 + 1, def.col0 + 1);
    // 定义点自身（includeDeclaration 回显；ref_tools 的 drop_self_reference 在
    // Windows 上斜杠方向失配恒不命中）不算 caller。container 空 = 顶层引用
    // （use 导入等），同样无人可导航。
    let mut callers: Vec<String> = refs
        .iter()
        .filter(|r| !(r.file == def.file && r.line == def.line0 && r.col == def.col0))
        .map(|r| r.container_name.clone())
        .filter(|c| !c.is_empty())
        .collect();
    callers.sort();
    callers.dedup();
    let ref_entries: Vec<Value> = refs
        .iter()
        .map(|r| {
            json!({
                // RefSymbolHit 携带 LSP 原始 0-based 行列 → 信封统一 1-based。
                "file": r.file, "line": r.line + 1, "col": r.col + 1,
                "container": r.container_name,
            })
        })
        .collect();
    let (snippets, snippets_skipped) = match sup
        .tool_referencing_code_snippets(
            root,
            &def.file,
            line - 1,
            col - 1,
            SNIPPET_CONTEXT_LINES,
            CALLER_MAX_REFS,
            None,
        )
        .await
    {
        // 第二 = 截断旗，第三 = aap4 快照（CT 聚合视图均不消费）。
        Ok((hits, _, _)) => (
            hits.iter()
                .map(|s| {
                    json!({
                        // LSP 原始 0-based 行列 → 信封统一 1-based。
                        "file": s.file, "line": s.line + 1, "col": s.col + 1,
                        "text": s.text,
                    })
                })
                .collect::<Vec<_>>(),
            None,
        ),
        Err(e) => (Vec::new(), Some(format!("snippets skipped: {e}"))),
    };
    let mut env = json!({
        "symbol": symbol,
        "definition": { "file": def.file, "line": line, "col": col },
        "callers_count": callers.len(),
        "callers": callers,
        "refs": ref_entries,
        "snippets": snippets,
    });
    if let Some(note) = snippets_skipped {
        env["snippets_skipped"] = json!(note);
    }
    let budget = budget_bytes(GOTO_CALLERS_BUDGET_TOKENS);
    crate::recipe::truncate_list_field(&mut env, "refs", budget);
    crate::recipe::truncate_list_field(&mut env, "snippets", budget);
    if env_len(&env) > budget {
        clamp_json_strings(&mut env, HOVER_MAX_CHARS);
    }
    Ok(env)
}

/// `ct_smart_edit <file> <sym> <transform> <new_name>`：CT-3 两义定死。
/// rename = 委托既有 LSP rename-symbol（prepareRename 拒绝 → 确定性能力错）；
/// extract = 仅整符号抽取（fn 提为同级新 fn，原签名保留、body 替换为调用）。
pub(crate) async fn ct_smart_edit(
    sup: &crate::Supervisor,
    root: &Path,
    file: &str,
    symbol: &str,
    transform: &str,
    new_name: &str,
) -> Result<Value, ToolError> {
    if file.is_empty() || symbol.is_empty() {
        return Err(ToolError::BadArgs {
            detail: "file and symbol must not be empty".into(),
        });
    }
    if new_name.is_empty() || new_name.contains(' ') {
        return Err(ToolError::BadArgs {
            detail: "new_name must be non-empty, no whitespace".into(),
        });
    }
    if new_name == symbol {
        return Err(ToolError::BadArgs {
            detail: "new_name must differ from symbol".into(),
        });
    }
    if !matches!(transform, "rename" | "extract") {
        return Err(ToolError::BadArgs {
            detail: format!("transform must be `rename` or `extract`, got {transform:?}"),
        });
    }
    let (payload, txn_id) = ct_txn(root, async {
        match transform {
            "rename" => smart_edit_rename(sup, root, file, symbol, new_name).await,
            _ => smart_edit_extract(sup, root, file, symbol, new_name).await,
        }
    })
    .await?;
    let mut env = json!({
        "transform": transform,
        "file": file,
        "symbol": symbol,
        "new_name": new_name,
        "txn_id": txn_id,
    });
    for (k, v) in payload {
        env[k] = v;
    }
    let budget = budget_bytes(SMART_EDIT_BUDGET_TOKENS);
    crate::recipe::truncate_list_field(&mut env, "files", budget);
    crate::recipe::truncate_list_field(&mut env, "skipped", budget);
    if env_len(&env) > budget {
        clamp_json_strings(&mut env, HOVER_MAX_CHARS);
    }
    Ok(env)
}

/// rename 分支：documentSymbol 定位 item → 名字 token 行列 → LSP rename。
/// prepareRename 拒绝（LS 无 rename 能力/位置不可改名）→ CT-3 确定性错误。
async fn smart_edit_rename(
    sup: &crate::Supervisor,
    root: &Path,
    file: &str,
    symbol: &str,
    new_name: &str,
) -> Result<Vec<(String, Value)>, ToolError> {
    let hits = sup.tool_overview(root, file, None).await?;
    let Some(h) = hits.iter().find(|h| h.name == symbol) else {
        return Err(ToolError::BadArgs {
            detail: format!("symbol `{symbol}` not found in {file}"),
        });
    };
    let def = DefHit {
        file: file.to_string(),
        line0: h.range.start.line,
        col0: h.range.start.character,
    };
    let (line, col) = name_position(sup, root, symbol, &def).await;
    // tool_rename_symbol 入口按 0-based LSP position 直传（lsp_position_from_byte
    // 不做 -1；1-based→0-based 换算历来在 CLI 层）。
    let report = match sup
        .tool_rename_symbol(root, file, line - 1, col - 1, new_name, None)
        .await
    {
        Ok(r) => r,
        Err(ToolError::BadArgs { detail }) if detail.contains("prepareRename") => {
            return Err(unsupported("rename", detail));
        }
        Err(e) => return Err(e),
    };
    let files = report.files.iter().cloned().map(Value::String).collect();
    let skipped: Vec<Value> = report
        .skipped
        .iter()
        .map(|s| json!({ "file": s.file, "reason": s.reason }))
        .collect();
    Ok(vec![
        ("files_modified".into(), json!(report.files_modified)),
        ("edits_applied".into(), json!(report.edits_applied)),
        ("files".into(), files),
        ("skipped".into(), Value::Array(skipped)),
    ])
}

/// extract 分支（仅整符号抽取）：item 文本内解析签名 → 同级新 fn（原 item
/// 文本换名）+ 原 fn body 替换为调用。写步走既有 insert-after-symbol 与
/// replace-body（写门 + didChange + recorded_write 内建），同一事务聚合。
async fn smart_edit_extract(
    sup: &crate::Supervisor,
    root: &Path,
    file: &str,
    symbol: &str,
    new_name: &str,
) -> Result<Vec<(String, Value)>, ToolError> {
    let item = sup.tool_symbol_body(root, file, symbol, None).await?;
    let Some((name_off, _, _)) = locate_signature_name(&item, symbol) else {
        // bd P2-6：本实现按 Rust 语法解析签名（fn 关键字 + 参数串重建）。非 rust
        // 文件的 def 落到这里时，旧文案 "`x` is not a function item" 张冠李戴
        // （python def 明明是函数）——真实原因 = 该重构仅实现了 Rust 源。
        let lang = crate::resolve_lang_for_file(file, None)
            .unwrap_or_else(|_| "unknown".to_string());
        if lang != "rust" {
            return Err(unsupported(
                "extract",
                format!(
                    "extract transform supports Rust sources only; the {lang} LS does not provide this refactoring"
                ),
            ));
        }
        return Err(unsupported(
            "extract",
            format!("`{symbol}` is not a function item"),
        ));
    };
    let Some((params, body_open)) = split_fn_params_body(&item, name_off) else {
        return Err(unsupported(
            "extract",
            format!("`{symbol}` has no body (trait declaration?)"),
        ));
    };
    let Some(bindings) = param_bindings(&params) else {
        return Err(unsupported(
            "extract",
            format!("`{symbol}` has parameters whose call shape cannot be rebuilt"),
        ));
    };
    // 新 fn = 原 item 文本换签名名（可见性/泛型/where 原样保留）。
    let new_fn = format!(
        "{}{}{}",
        &item[..name_off],
        new_name,
        &item[name_off + symbol.len()..]
    );
    // 原 fn = 原签名 + body 换为调用（缩进沿用 body 首行风格）。
    let call = build_call_expr(new_name, &bindings);
    let indent = body_indent(&item, body_open);
    let original = format!(
        "{}{{\n{indent}{call}\n}}",
        &item[..body_open],
    );
    sup.tool_edit_insert_after_symbol(root, file, symbol, &format!("\n\n{new_fn}"), false, None)
        .await?;
    sup.tool_replace_body(root, file, symbol, &original, None).await?;
    Ok(vec![
        (
            "new_fn".into(),
            json!({ "name": new_name, "inserted_after": symbol }),
        ),
        ("call".into(), json!({ "expr": call })),
    ])
}

/// `ct_pick <query>`：模糊候选（整 query + 逐 token 各拉一次 workspace/symbol，
/// (name,file,line) 去重；得分 = 名字命中的 token 数，降序 → 名短优先）。
/// AI 消费形态：candidates 带 1-based pick 号，1 token 回复。
pub(crate) async fn ct_pick(
    sup: &crate::Supervisor,
    root: &Path,
    query: &str,
) -> Result<Value, ToolError> {
    let query = query.trim();
    if query.is_empty() {
        return Err(ToolError::BadArgs {
            detail: "query must not be empty".into(),
        });
    }
    let tokens: Vec<&str> = query.split_whitespace().collect();
    struct Cand {
        name: String,
        kind: Value,
        file: String,
        line: u32,
        score: usize,
    }
    let mut cands: Vec<Cand> = Vec::new();
    let mut queries: Vec<String> = vec![query.to_string()];
    queries.extend(tokens.iter().map(|t| t.to_string()));
    let mut query_errors: Vec<String> = Vec::new();
    for q in &queries {
        match sup.tool_find_symbol(root, q, PICK_QUERY_LIMIT, None).await {
            Ok((items, _, _)) => {
                for h in items {
                    let name = h.name.clone();
                    let file = crate::uri_to_path(&h.uri)
                        .map(|p| crate::recipe::rel_forward(root, &p.to_string_lossy()))
                        .unwrap_or_else(|| h.uri.clone());
                    let line = h.range.start.line + 1;
                    if cands
                        .iter()
                        .any(|c| c.name == name && c.file == file && c.line == line)
                    {
                        continue;
                    }
                    let name_lc = name.to_ascii_lowercase();
                    let score = tokens
                        .iter()
                        .filter(|t| name_lc.contains(&t.to_ascii_lowercase()))
                        .count();
                    cands.push(Cand {
                        name,
                        kind: serde_json::to_value(&h.kind).unwrap_or(Value::Null),
                        file,
                        line,
                        score,
                    });
                }
            }
            Err(e) => query_errors.push(format!("{q}: {e}")),
        }
    }
    if cands.is_empty() && !query_errors.is_empty() {
        // 全 query 失败 = 上抛（错误不静默）；部分失败容忍（聚合视图）。
        return Err(ToolError::BadArgs {
            detail: format!("pick: all queries failed: {}", query_errors.join("; ")),
        });
    }
    cands.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then(a.name.len().cmp(&b.name.len()))
            .then(a.name.cmp(&b.name))
            .then(a.file.cmp(&b.file))
    });
    let candidates: Vec<Value> = cands
        .iter()
        .take(PICK_MAX_CANDIDATES)
        .enumerate()
        .map(|(i, c)| {
            json!({
                "pick": i + 1,
                "name": c.name,
                "kind": c.kind.clone(),
                "file": c.file,
                "line": c.line,
                "score": c.score,
            })
        })
        .collect();
    let mut env = json!({
        "query": query,
        "candidates_count": cands.len(),
        "candidates": candidates,
        "pick_hint": "reply with one token: the `pick` number of the best candidate",
    });
    if !query_errors.is_empty() {
        env["query_errors"] = json!(query_errors);
    }
    let budget = budget_bytes(PICK_BUDGET_TOKENS);
    crate::recipe::truncate_list_field(&mut env, "candidates", budget);
    Ok(env)
}

/// 单文件写入（write-with-tests 用）：新文件走 create-text-file，已存在走
/// replace-lines 全量覆盖（空文件 insert-at-line）——三者都内建写门 + didChange
/// + recorded_write（入当前 CT 事务）。
pub(crate) async fn write_via_tool(
    sup: &crate::Supervisor,
    root: &Path,
    file: &str,
    content: &str,
) -> Result<(), ToolError> {
    let abs =
        crate::path_guard::guarded_join(root, file).map_err(|detail| ToolError::BadArgs {
            detail,
        })?;
    if !abs.exists() {
        return sup.tool_create_text_file(root, file, content, None).await.map(|_| ());
    }
    let total = tokio::fs::read_to_string(&abs)
        .await
        .map_err(|e| ToolError::BadArgs {
            detail: format!("read {file}: {e}"),
        })?
        .lines()
        .count() as u32;
    if total == 0 {
        sup.tool_insert_at_line(root, file, 1, content, None, None)
            .await
            .map(|_| ())
    } else {
        sup.tool_replace_lines(root, file, 1, total, content, None, None)
            .await
    }
}

/// `ct_write_with_tests <file> <code> <test_file> <tests> [name]`：事务内只含
/// 写步（两文件），测试执行在事务外（计划 §1.7：runner 产物不进 undo/txn 栈）；
/// 报告 = txn_id + 写清单 + run_test 信封。
pub(crate) async fn ct_write_with_tests(
    sup: &crate::Supervisor,
    root: &Path,
    file: &str,
    code: &str,
    test_file: &str,
    tests: &str,
    test_name: Option<&str>,
) -> Result<Value, ToolError> {
    if file.is_empty() || test_file.is_empty() {
        return Err(ToolError::BadArgs {
            detail: "file and test_file must not be empty".into(),
        });
    }
    if code.is_empty() {
        return Err(ToolError::BadArgs {
            detail: "code must not be empty".into(),
        });
    }
    // 父目录脚手架：mkdir 不入 undo 账（undo 按文件快照，created 文件回滚 =
    // 删文件，空目录残留无害）。
    for f in [file, test_file] {
        if let Some(parent) = root.join(f).parent() {
            std::fs::create_dir_all(parent).map_err(|e| ToolError::BadArgs {
                detail: format!("mkdir {}: {e}", parent.display()),
            })?;
        }
    }
    let (wrote, txn_id) = ct_txn(root, async {
        let mut wrote = Vec::new();
        for (f, content) in [(file, code), (test_file, tests)] {
            write_via_tool(sup, root, f, content).await?;
            wrote.push(json!({ "file": f }));
        }
        Ok(wrote)
    })
    .await?;
    let test = crate::recipe::run_test(root, test_file, test_name).await?;
    let mut env = json!({
        "file": file,
        "test_file": test_file,
        "txn_id": txn_id,
        "wrote": wrote,
        "test": test,
    });
    fit_write_tests(&mut env);
    Ok(env)
}

/// write-with-tests 信封预算收缩：test.failures 逐条回退（test.backend/passed/
/// failed 骨架远小于预算）→ wrote 截条 → 字符串 clamp。
fn fit_write_tests(env: &mut Value) {
    let budget = budget_bytes(WRITE_TESTS_BUDGET_TOKENS);
    shrink_nested_list(env, "test", "failures", budget);
    if env_len(env) > budget {
        crate::recipe::truncate_list_field(env, "wrote", budget);
    }
    if env_len(env) > budget {
        clamp_json_strings(env, HOVER_MAX_CHARS);
    }
}

/// `ct_define_feature <name> [file]`：找已有（存在 → caller 检索 + find-test
/// 报告，不写 stub——覆盖既有符号 = 破坏，显式记 stub_skipped）；新名 → 写
/// stub（仅 .rs 目标，确定性拒绝其他扩展名；file 缺席 → stub 文本带出不落盘）。
pub(crate) async fn ct_define_feature(
    sup: &crate::Supervisor,
    root: &Path,
    name: &str,
    file: Option<&str>,
) -> Result<Value, ToolError> {
    if name.is_empty() {
        return Err(ToolError::BadArgs {
            detail: "name must not be empty".into(),
        });
    }
    let (items, _, _) = sup.tool_find_symbol(root, name, 10, None).await?;
    if let Some(hit) = items.iter().find(|i| i.name == name) {
        let Some(path) = crate::uri_to_path(&hit.uri) else {
            return Err(ToolError::BadArgs {
                detail: format!("symbol `{name}` has non-file uri: {}", hit.uri),
            });
        };
        let def = DefHit {
            file: crate::recipe::rel_forward(root, &path.to_string_lossy()),
            line0: hit.range.start.line,
            col0: hit.range.start.character,
        };
        // workspace/symbol location 即标识符位置（0-based 直传）；信封 1-based。
        let (refs, _aap4) = sup
            .tool_referencing_symbols(root, &def.file, def.line0, def.col0, None)
            .await?;
        let (line, col) = (def.line0 + 1, def.col0 + 1);
        // 定义点自身回显不算 caller（同 ct_goto_callers 注）。
        let mut callers: Vec<String> = refs
            .iter()
            .filter(|r| !(r.file == def.file && r.line == def.line0 && r.col == def.col0))
            .map(|r| r.container_name.clone())
            .filter(|c| !c.is_empty())
            .collect();
        callers.sort();
        callers.dedup();
        let find_test = crate::recipe::find_test(sup, root, name).await?;
        let mut env = json!({
            "name": name,
            "exists": true,
            "definition": { "file": def.file, "line": line, "col": col },
            "callers_count": callers.len(),
            "callers": callers,
            "find_test": find_test,
            "stub_skipped": "symbol already exists",
        });
        fit_define_feature(&mut env);
        return Ok(env);
    }
    let Some(target) = file else {
        return Ok(json!({
            "name": name,
            "exists": false,
            "stub": { "written": false, "content": rust_stub(name) },
            // 批2-F：文案对齐实际行为——本步不落盘；落盘方是 recipe add-feature
            // 的 write-stub 步（--target），或调用方自选途径，并非固定 create-text-file。
            "stub_note": "stub not written; content is in stub.content — recipe add-feature writes it to --target, otherwise create the file yourself",
        }));
    };
    if !target.ends_with(".rs") {
        return Err(ToolError::BadArgs {
            detail: format!("stub generation supports .rs targets only, got {target}"),
        });
    }
    let stub = rust_stub(name);
    let (_, txn_id) = ct_txn(root, async {
        let abs = crate::path_guard::guarded_join(root, target)
            .map_err(|detail| ToolError::BadArgs { detail })?;
        if abs.exists() {
            // 追加 EOF：insert-at-line(total+1)；结尾无换行先补分隔。
            let existing = tokio::fs::read_to_string(&abs)
                .await
                .map_err(|e| ToolError::BadArgs {
                    detail: format!("read {target}: {e}"),
                })?;
            let sep = if existing.ends_with('\n') { "" } else { "\n" };
            let lines = existing.lines().count() as u32;
            sup.tool_insert_at_line(root, target, lines + 1, &format!("{sep}{stub}"), None, None)
                .await?;
        } else {
            if let Some(parent) = abs.parent() {
                std::fs::create_dir_all(parent).map_err(|e| ToolError::BadArgs {
                    detail: format!("mkdir {}: {e}", parent.display()),
                })?;
            }
            sup.tool_create_text_file(root, target, &stub, None).await?;
        }
        Ok(())
    })
    .await?;
    Ok(json!({
        "name": name,
        "exists": false,
        "txn_id": txn_id,
        "stub": { "written": true, "file": target, "content": stub },
    }))
}

/// define-feature 信封预算收缩（exists 分支 refs/find_test 可能超）。
fn fit_define_feature(env: &mut Value) {
    let budget = budget_bytes(DEFINE_FEATURE_BUDGET_TOKENS);
    shrink_nested_list(env, "find_test", "hits", budget);
    if env_len(env) > budget {
        crate::recipe::truncate_list_field(env, "callers", budget);
    }
    if env_len(env) > budget {
        clamp_json_strings(env, HOVER_MAX_CHARS);
    }
}

/// `ct_review_diff [txn-id]`：diff（批1 diff_txn，None = 最近事务）→ diff 内
/// 文件逐个 find-test 关联（前 N 文件封顶）→ 首个命中测试文件跑测 → 报告。
/// 无关联测试显式记 test_skipped（不静默、不冒充全绿）。
pub(crate) async fn ct_review_diff(
    sup: &crate::Supervisor,
    root: &Path,
    txn_id: Option<u64>,
) -> Result<Value, ToolError> {
    let diff = crate::recipe::diff_txn(root, txn_id, false).await?;
    let mut related: Vec<Value> = Vec::new();
    let mut first_test_file: Option<String> = None;
    if let Some(files) = diff.get("files").and_then(Value::as_array) {
        for f in files.iter().take(REVIEW_DIFF_MAX_FILES) {
            let Some(path) = f.get("path").and_then(Value::as_str) else {
                continue;
            };
            let stem = file_stem(path);
            if stem.is_empty() {
                continue;
            }
            let ft = crate::recipe::find_test(sup, root, &stem).await?;
            let hits = ft
                .get("hits")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if let Some(tf) = hits
                .first()
                .and_then(|h| h.get("file"))
                .and_then(Value::as_str)
                .map(str::to_owned)
                && first_test_file.is_none()
            {
                first_test_file = Some(tf);
            }
            if !hits.is_empty() {
                related.push(json!({ "path": path, "find_test": ft }));
            }
        }
    }
    let (test, test_skipped) = match first_test_file.as_deref() {
        Some(tf) => (Some(crate::recipe::run_test(root, tf, None).await?), None),
        None => (
            None,
            Some("no related test hits; test run skipped".to_string()),
        ),
    };
    let mut env = json!({
        "txn_id": diff.get("txn_id").cloned().unwrap_or(Value::Null),
        "timestamp": diff.get("timestamp").cloned().unwrap_or(Value::Null),
        "files": diff.get("files").cloned().unwrap_or_else(|| json!([])),
        "related_tests": related,
        "test": test,
    });
    if let Some(note) = test_skipped {
        env["test_skipped"] = json!(note);
    }
    let budget = budget_bytes(REVIEW_DIFF_BUDGET_TOKENS);
    shrink_nested_list(&mut env, "test", "failures", budget);
    if env_len(&env) > budget {
        crate::recipe::truncate_list_field(&mut env, "files", budget);
    }
    if env_len(&env) > budget {
        crate::recipe::truncate_list_field(&mut env, "related_tests", budget);
    }
    Ok(env)
}

// ============ 预算裁剪共用件 ============

/// 信封序列化字节数。
fn env_len(v: &Value) -> usize {
    serde_json::to_vec(v).map(|b| b.len()).unwrap_or(usize::MAX)
}

/// 字符串节按整行收缩到预算内（head 类字段；断行内容不可消费，整行保留）。
/// 裁剪 → 顶层写 truncated:true + original_count（原总行数，§10-G 词汇）。
/// 返回是否裁剪。
fn shrink_lines_field(env: &mut Value, key: &str, budget: usize) -> bool {
    let Some(text) = env.get(key).and_then(Value::as_str).map(str::to_owned) else {
        return false;
    };
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return false;
    }
    let fits = |keep: usize| {
        let mut trial = env.clone();
        trial[key] = Value::String(lines[..keep].join("\n"));
        env_len(&trial) + 64 <= budget // +64：截断后补写的 truncated/original_count
    };
    let keep = (1..=lines.len()).rev().find(|&k| fits(k)).unwrap_or(0);
    let original = lines.len();
    env[key] = Value::String(lines[..keep].join("\n"));
    if let Some(obj) = env.as_object_mut() {
        obj.insert("truncated".into(), json!(true));
        obj.insert("original_count".into(), json!(original));
    }
    true
}

/// 嵌套 section.key 列表按整信封口径逐条回退（shrink_nested_hits 泛化版，批3
/// test.failures / find_test.hits 共用；+48 同款旗标开销余量）。旗标落 section 内。
fn shrink_nested_list(env: &mut Value, section: &str, key: &str, budget: usize) {
    let original = env
        .get(section)
        .and_then(|s| s.get(key))
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    let mut popped = 0usize;
    while env_len(env) + 48 > budget {
        let Some(arr) = env
            .get_mut(section)
            .and_then(|s| s.get_mut(key))
            .and_then(Value::as_array_mut)
        else {
            break;
        };
        if arr.pop().is_none() {
            break;
        }
        popped += 1;
    }
    if popped > 0
        && let Some(s) = env.get_mut(section)
    {
        s["truncated"] = json!(true);
        s["original_count"] = json!(original);
    }
}

#[cfg(test)]
mod tests;
