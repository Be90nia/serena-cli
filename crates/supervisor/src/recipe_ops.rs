//! recipe 编排层（local/recipe-plan.md §2 批4）：8 个预定义工作流收编为单命令
//! `recipe <name>`。上下文 = 程序化管道 [`RecipeCtx`]；读步无事务，写步各自
//! 独立 undo 事务（[`ct_txn`] 嵌套 TXN_UID scope 覆盖 execute_tool 外层 uid
//! —— 写记账若落在外层 uid，"recipe" 不在 WRITE_TOOLS 不收口，TxnGuard drop
//! 兜底 abort 会丢账：盘改了、undo 栈无记录）。单步 error → 失败即停 + 逆序
//! undo 已完成写步（回滚到 recipe 前）+ 报告 {completed_steps, txn_ids,
//! undo_results}（wire RPC_ERROR，码集不变）；单步截断（truncated）→ 继续
//! （截断是各步预算内自限，非错误）。

use lsp_core::docsync::path_to_uri_str;
use lsp_core::offsets::OffsetEncoding;
use serde_json::{Value, json};
use std::path::Path;

/// 管道上下文（计划 §2 批4 规格 RecipeCtx{file,sym,txn_ids,findings}）：
/// 步骤间的程序化数据流 + 编排运行账。
struct RecipeCtx {
    /// 当前主文件（fix-bug/rename/refactor-extract 的 verify 目标）。
    file: Option<String>,
    /// 当前主符号。
    sym: Option<String>,
    /// 已完成写步的 txn id（完成序；失败按逆序回滚）。
    txn_ids: Vec<u64>,
    /// findings = 已完成步（步名 → 信封，完成序；后续步与顶层响应读取）。
    findings: Vec<(String, Value)>,
}

impl RecipeCtx {
    fn new(file: Option<String>, sym: Option<String>) -> Self {
        Self {
            file,
            sym,
            txn_ids: Vec::new(),
            findings: Vec::new(),
        }
    }

    /// 按步名取产物（首个同名步）。
    fn take(&self, step: &str) -> Value {
        self.findings
            .iter()
            .find(|(n, _)| n == step)
            .map(|(_, v)| v.clone())
            .unwrap_or(Value::Null)
    }

    /// 读步：无事务；error 上抛由调用方转 fail。
    async fn read_step<F>(&mut self, name: &str, fut: F) -> Result<Value, crate::ToolError>
    where
        F: Future<Output = Result<Value, crate::ToolError>>,
    {
        let v = fut.await?;
        self.findings.push((name.to_string(), v.clone()));
        Ok(v)
    }

    /// 写步：独立 undo 事务（内嵌 TXN_UID scope + commit），txn_id 注入信封
    /// 并记账。测试执行类只读步（产物不进 undo 栈）走 read_step。
    /// fut 必须 Box::pin：write 闭包嵌 execute_tool→dispatch→recipe→ct_txn→
    /// tool_* 多层 async，tokio worker 线程栈（2MB）被嵌套 poll 压爆
    /// （daemon 启动读步可过、首个写步即栈溢出——SweepA3b dry_run 同款修法）。
    async fn write_step<F>(
        &mut self,
        name: &str,
        root: &Path,
        fut: F,
    ) -> Result<Value, crate::ToolError>
    where
        F: Future<Output = Result<Value, crate::ToolError>>,
    {
        let fut = Box::pin(fut);
        let (mut v, txn_id) = crate::ct::ct_txn(root, fut).await?;
        self.txn_ids.push(txn_id);
        if let Some(o) = v.as_object_mut() {
            o.insert("txn_id".into(), json!(txn_id));
        }
        self.findings.push((name.to_string(), v.clone()));
        Ok(v)
    }

    /// 失败收口：逆序 undo 已完成写步（回滚到 recipe 前），undo 失败逐条
    /// 显式记账不静默（盘上残留必须可见）。返回携带完整报告的 wire 错误。
    async fn fail(
        self,
        name: &str,
        root: &Path,
        failed_step: &str,
        err: crate::ToolError,
    ) -> crate::ToolError {
        let mut undo_results = Vec::new();
        if !self.txn_ids.is_empty() {
            // 与写工具串行化：恢复写期间不得有并发写改盘（undo_at 同款）。
            match crate::write_gate::acquire("recipe-undo").await {
                Ok(_gate) => match crate::undo::store_for(root) {
                    Ok(store) => {
                        for id in self.txn_ids.iter().rev() {
                            match crate::undo::undo_one(&store, *id).await {
                                Ok(files) => undo_results
                                    .push(json!({"txn_id": id, "undone_files": files})),
                                Err(e) => undo_results
                                    .push(json!({"txn_id": id, "error": e.to_string()})),
                            }
                        }
                    }
                    Err(_) => undo_results.push(json!({"error": "undo store unavailable"})),
                },
                Err(e) => undo_results.push(json!({"error": format!("write gate: {e}")})),
            }
        }
        protocol_fail(
            name,
            failed_step,
            self.findings,
            self.txn_ids,
            undo_results,
            &err,
        )
    }
}

/// 失败报告 → wire 错误（RPC_ERROR，码集不变；reason = 紧凑 JSON 报告）。
fn protocol_fail(
    name: &str,
    failed_step: &str,
    steps: Vec<(String, Value)>,
    txn_ids: Vec<u64>,
    undo_results: Vec<Value>,
    err: &crate::ToolError,
) -> crate::ToolError {
    crate::ToolError::Protocol {
        tool: format!("recipe:{name}"),
        reason: json!({
            "failed_step": failed_step,
            "error": err.to_string(),
            "completed_steps": steps.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>(),
            "step_results": steps,
            "txn_ids": txn_ids,
            "undo_results": undo_results,
        })
        .to_string(),
    }
}

/// `recipe <name>` 入口：解析 name/位置参数/flags，分派 8 recipe。
/// 位置参数按 recipe 语义解释（pos 数组原样传入，daemon 侧集中解释——CLI 层零语义）。
pub(crate) async fn run(
    sup: &crate::Supervisor,
    root: &Path,
    args: &Value,
) -> Result<Value, crate::ToolError> {
    let name = args
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| crate::ToolError::BadArgs {
            detail: "missing 'name' (fix-bug|add-feature|rename|add-test|refactor-extract|refactor-rename|review-diff|explore)".into(),
        })?;
    let pos: Vec<String> = args
        .get("pos")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let flag = |k: &str| args.get(k).and_then(Value::as_str);
    match name {
        "fix-bug" => {
            let (file, sym) = two_pos(&pos, name, ("file", "sym"))?;
            fix_bug(sup, root, &file, &sym, flag("new_body")).await
        }
        "add-feature" => {
            let nm = one_pos(&pos, name, "name")?;
            add_feature(sup, root, &nm, args.get("target").and_then(Value::as_str), args).await
        }
        "rename" => {
            let (file, sym) = two_pos(&pos, name, ("file", "sym"))?;
            rename(sup, root, &file, &sym, flag_new_name(args, name)?).await
        }
        "add-test" => {
            let sym = one_pos(&pos, name, "sym")?;
            add_test(sup, root, &sym, args.get("run").and_then(Value::as_bool).unwrap_or(false)).await
        }
        "refactor-extract" => {
            let (file, sym) = two_pos(&pos, name, ("file", "sym"))?;
            refactor_extract(sup, root, &file, &sym, flag_new_name(args, name)?).await
        }
        "refactor-rename" => {
            let sym = one_pos(&pos, name, "sym")?;
            refactor_rename(sup, root, &sym, flag_new_name(args, name)?).await
        }
        "review-diff" => {
            let txn_id = pos.first().and_then(|s| s.parse::<u64>().ok());
            review_diff(sup, root, txn_id).await
        }
        "explore" => {
            let path = one_pos(&pos, name, "path")?;
            explore(sup, root, &path).await
        }
        other => Err(crate::ToolError::BadArgs {
            detail: format!(
                "unknown recipe `{other}`; available: fix-bug add-feature rename add-test refactor-extract refactor-rename review-diff explore"
            ),
        }),
    }
}

fn one_pos(pos: &[String], name: &str, what: &str) -> Result<String, crate::ToolError> {
    pos.first().cloned().ok_or_else(|| crate::ToolError::BadArgs {
        detail: format!("recipe {name}: missing positional arg <{what}>"),
    })
}

fn two_pos(
    pos: &[String],
    name: &str,
    (a, b): (&str, &str),
) -> Result<(String, String), crate::ToolError> {
    if pos.len() < 2 {
        return Err(crate::ToolError::BadArgs {
            detail: format!("recipe {name}: expected positional args <{a}> <{b}>, got {}", pos.len()),
        });
    }
    Ok((pos[0].clone(), pos[1].clone()))
}

/// --to（rename/refactor-rename）与 --as（refactor-extract）按 recipe 取新名。
fn flag_new_name(args: &Value, name: &str) -> Result<String, crate::ToolError> {
    let key = if name == "refactor-extract" { "as" } else { "to" };
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| crate::ToolError::BadArgs {
            detail: format!("recipe {name}: missing --{key} <NEW_NAME>"),
        })
}

// ============ 8 recipe（计划 §2 批4 规格表） ============

/// fix-bug `<file> <sym> [--new-body T]`：
/// ct_tldr → ct_goto_callers → ct_verify(前) → [replace-body 写] → ct_verify(后)。
/// 无 --new-body 时只做分析链（无写步，无 after 段）。
async fn fix_bug(
    sup: &crate::Supervisor,
    root: &Path,
    file: &str,
    sym: &str,
    new_body: Option<&str>,
) -> Result<Value, crate::ToolError> {
    let mut ctx = RecipeCtx::new(Some(file.into()), Some(sym.into()));
    let file_s = ctx.file.clone().expect("constructor sets file");
    let sym_s = ctx.sym.clone().expect("constructor sets sym");
    if let Err(e) = ctx
        .read_step("ct_tldr", crate::ct::ct_tldr(sup, root, &file_s))
        .await
    {
        return Err(ctx.fail("fix-bug", root, "ct_tldr", e).await);
    }
    if let Err(e) = ctx
        .read_step("ct_goto_callers", crate::ct::ct_goto_callers(sup, root, &sym_s))
        .await
    {
        return Err(ctx.fail("fix-bug", root, "ct_goto_callers", e).await);
    }
    if let Err(e) = ctx
        .read_step("ct_verify_before", crate::ct::ct_verify(sup, root, &file_s, None))
        .await
    {
        return Err(ctx.fail("fix-bug", root, "ct_verify_before", e).await);
    }
    let Some(body) = new_body.filter(|b| !b.is_empty()) else {
        return Ok(json!({
            "recipe": "fix-bug",
            "mode": "analyze-only",
            "no_write_note": "--new-body absent; write step skipped",
            "steps": into_steps(&ctx),
        }));
    };
    let body_s = body.to_string();
    let f2 = file_s.clone();
    let s2 = sym_s.clone();
    if let Err(e) = ctx
        .write_step("replace-body", root, async move {
            sup.tool_replace_body(root, &f2, &s2, &body_s, None).await?;
            Ok(json!({"file": f2, "symbol": s2}))
        })
        .await
    {
        return Err(ctx.fail("fix-bug", root, "replace-body", e).await);
    }
    let txn = ctx.txn_ids.last().copied();
    let after = match ctx
        .read_step("ct_verify_after", crate::ct::ct_verify(sup, root, &file_s, txn))
        .await
    {
        Ok(v) => v,
        Err(e) => return Err(ctx.fail("fix-bug", root, "ct_verify_after", e).await),
    };
    Ok(json!({
        "recipe": "fix-bug",
        "file": file_s,
        "symbol": sym_s,
        "txn_id": txn,
        "verify_before": ctx.take("ct_verify_before"),
        "verify_after": after,
        "steps": into_steps(&ctx),
    }))
}

/// add-feature `<name> [--target F] [--tests-file T --tests C]`：
/// ct_define_feature(报告/生成 stub 文本，不落盘) → [stub 落盘写（独立 txn）]
/// → 可选 ct_write_with_tests。exists → stub_skipped（无写步）。
async fn add_feature(
    sup: &crate::Supervisor,
    root: &Path,
    name: &str,
    target: Option<&str>,
    args: &Value,
) -> Result<Value, crate::ToolError> {
    let mut ctx = RecipeCtx::new(target.map(str::to_string), None);
    let defined = match ctx
        .read_step("ct_define_feature", crate::ct::ct_define_feature(sup, root, name, None))
        .await
    {
        Ok(v) => v,
        Err(e) => return Err(ctx.fail("add-feature", root, "ct_define_feature", e).await),
    };
    if defined.get("exists").and_then(Value::as_bool) == Some(true) {
        return Ok(json!({
            "recipe": "add-feature",
            "name": name,
            "definition": defined,
            "steps": into_steps(&ctx),
        }));
    }
    let Some(target) = target.filter(|t| !t.is_empty()) else {
        return Ok(json!({
            "recipe": "add-feature",
            "name": name,
            "stub_not_written": "--target absent; stub content in steps[0]",
            "steps": into_steps(&ctx),
        }));
    };
    let stub = defined
        .get("stub")
        .and_then(|s| s.get("content"))
        .and_then(Value::as_str)
        .ok_or_else(|| crate::ToolError::BadArgs {
            detail: "ct_define_feature returned no stub content".into(),
        })?
        .to_string();
    let stub_len = stub.len();
    let target_s = target.to_string();
    if let Err(e) = ctx
        .write_step("write-stub", root, async {
            append_or_create(sup, root, &target_s, &stub).await?;
            Ok(json!({"file": target_s, "bytes": stub_len}))
        })
        .await
    {
        return Err(ctx.fail("add-feature", root, "write-stub", e).await);
    }
    let txn = ctx.txn_ids.last().copied();
    // 可选测试步：--tests-file + --tests 同时给定时写测试并跑（测试产物不进
    // undo 栈，计划 §1.7；测试写步自身有独立 txn）。
    let tests_file = args.get("tests_file").and_then(Value::as_str);
    let tests = args.get("tests").and_then(Value::as_str);
    let test = match (tests_file, tests) {
        (Some(tf), Some(tc)) if !tf.is_empty() && !tc.is_empty() => {
            let code = stub.clone();
            match ctx
                .read_step(
                    "ct_write_with_tests",
                    crate::ct::ct_write_with_tests(sup, root, &target_s, &code, tf, tc, None),
                )
                .await
            {
                Ok(v) => Some(v),
                Err(e) => return Err(ctx.fail("add-feature", root, "ct_write_with_tests", e).await),
            }
        }
        _ => None,
    };
    Ok(json!({
        "recipe": "add-feature",
        "name": name,
        "target": target_s,
        "txn_id": txn,
        "test": test,
        "steps": into_steps(&ctx),
    }))
}

/// rename `<file> <sym> --to N`：
/// ct_impact → [LSP rename 单文件写（跨文件 edits 过滤进 skipped，93q 先例）]
/// → ct_verify。
async fn rename(
    sup: &crate::Supervisor,
    root: &Path,
    file: &str,
    sym: &str,
    new_name: String,
) -> Result<Value, crate::ToolError> {
    let mut ctx = RecipeCtx::new(Some(file.into()), Some(sym.into()));
    let file_s = ctx.file.clone().expect("constructor sets file");
    let sym_s = ctx.sym.clone().expect("constructor sets sym");
    if let Err(e) = ctx
        .read_step("ct_impact", crate::ct::ct_impact(sup, root, &sym_s))
        .await
    {
        return Err(ctx.fail("rename", root, "ct_impact", e).await);
    }
    let nn = new_name.clone();
    if let Err(e) = ctx
        .write_step("rename-in-file", root, async move {
            rename_in_file(sup, root, &file_s, &sym_s, &nn).await
        })
        .await
    {
        return Err(ctx.fail("rename", root, "rename-in-file", e).await);
    }
    let txn = ctx.txn_ids.last().copied();
    let after = match ctx
        .read_step("ct_verify_after", crate::ct::ct_verify(sup, root, file, txn))
        .await
    {
        Ok(v) => v,
        Err(e) => return Err(ctx.fail("rename", root, "ct_verify_after", e).await),
    };
    Ok(json!({
        "recipe": "rename",
        "file": file,
        "symbol": sym,
        "new_name": new_name,
        "txn_id": txn,
        "impact": ctx.take("ct_impact"),
        "rename": ctx.take("rename-in-file"),
        "verify_after": after,
        "steps": into_steps(&ctx),
    }))
}

/// add-test `<sym> [--run]`：
/// find-test → 命中即报告（已有测试，不写）；未命中 → 定位定义 → 定义文件 EOF
/// append `#[cfg(test)] mod tests` 模板（已有 mod tests → append_skipped 显式
/// 报告，不猜插入点）；--run 时跑 test 后端（只读，产物不进 undo 栈）。
async fn add_test(
    sup: &crate::Supervisor,
    root: &Path,
    sym: &str,
    run_tests: bool,
) -> Result<Value, crate::ToolError> {
    let mut ctx = RecipeCtx::new(None, Some(sym.into()));
    let sym_s = ctx.sym.clone().expect("constructor sets sym");
    let found = match ctx
        .read_step("find-test", crate::recipe::find_test(sup, root, &sym_s))
        .await
    {
        Ok(v) => v,
        Err(e) => return Err(ctx.fail("add-test", root, "find-test", e).await),
    };
    let hits = found.get("hits").and_then(Value::as_array);
    if hits.is_some_and(|h| !h.is_empty()) {
        return Ok(json!({
            "recipe": "add-test",
            "symbol": sym_s,
            "existing_tests": found,
            "steps": into_steps(&ctx),
        }));
    }
    let def = crate::ct::def_hit(sup, root, &sym_s).await?;
    if !def.file.ends_with(".rs") {
        return Err(crate::ToolError::BadArgs {
            detail: format!(
                "add-test template supports .rs targets only, definition is in {}",
                def.file
            ),
        });
    }
    let abs = crate::path_guard::guarded_join(root, &def.file)
        .map_err(|detail| crate::ToolError::BadArgs { detail })?;
    let existing = tokio::fs::read_to_string(&abs)
        .await
        .map_err(|e| crate::ToolError::BadArgs {
            detail: format!("read {}: {e}", def.file),
        })?;
    if existing.contains("mod tests") {
        return Ok(json!({
            "recipe": "add-test",
            "symbol": sym_s,
            "append_skipped": format!("{} already has `mod tests`; place the #[test] manually", def.file),
            "steps": into_steps(&ctx),
        }));
    }
    let template = format!(
        "\n#[cfg(test)]\nmod tests {{\n    use super::*;\n\n    #[test]\n    fn {sym_s}_works() {{\n        todo!(\"implement {sym_s}\")\n    }}\n}}\n"
    );
    let f2 = def.file.clone();
    let t2 = template.clone();
    if let Err(e) = ctx
        .write_step("append-test-template", root, async move {
            append_at_eof(sup, root, &f2, &t2).await?;
            Ok(json!({"file": f2}))
        })
        .await
    {
        return Err(ctx.fail("add-test", root, "append-test-template", e).await);
    }
    let txn = ctx.txn_ids.last().copied();
    let test = if run_tests {
        let dir = def
            .file
            .split('/')
            .next()
            .filter(|d| !d.is_empty())
            .unwrap_or(".")
            .to_string();
        match ctx
            .read_step("test", crate::recipe::run_test(root, &dir, None))
            .await
        {
            Ok(v) => Some(v),
            Err(e) => return Err(ctx.fail("add-test", root, "test", e).await),
        }
    } else {
        None
    };
    Ok(json!({
        "recipe": "add-test",
        "symbol": sym_s,
        "file": def.file,
        "txn_id": txn,
        "test": test,
        "steps": into_steps(&ctx),
    }))
}

/// refactor-extract `<file> <sym> --as N`：ct_smart_edit(extract，内部独立
/// txn) → ct_verify。
async fn refactor_extract(
    sup: &crate::Supervisor,
    root: &Path,
    file: &str,
    sym: &str,
    new_name: String,
) -> Result<Value, crate::ToolError> {
    let mut ctx = RecipeCtx::new(Some(file.into()), Some(sym.into()));
    let file_s = ctx.file.clone().expect("constructor sets file");
    let sym_s = ctx.sym.clone().expect("constructor sets sym");
    let smart = match ctx
        .write_step("ct_smart_edit_extract", root, async move {
            crate::ct::ct_smart_edit(sup, root, &file_s, &sym_s, "extract", &new_name).await
        })
        .await
    {
        Ok(v) => v,
        Err(e) => return Err(ctx.fail("refactor-extract", root, "ct_smart_edit_extract", e).await),
    };
    let txn = ctx.txn_ids.last().copied();
    let vf = ctx.file.clone().expect("constructor sets file");
    let after = match ctx
        .read_step("ct_verify_after", crate::ct::ct_verify(sup, root, &vf, txn))
        .await
    {
        Ok(v) => v,
        Err(e) => return Err(ctx.fail("refactor-extract", root, "ct_verify_after", e).await),
    };
    Ok(json!({
        "recipe": "refactor-extract",
        "file": vf,
        "symbol": sym,
        "txn_id": txn,
        "smart_edit": smart,
        "verify_after": after,
        "steps": into_steps(&ctx),
    }))
}

/// refactor-rename `<sym> --to N`：ct_impact → [LSP rename workspace 写（跨
/// 文件一个事务）] → ct_verify（定义文件）。smart_edit rename 分支同款定位。
async fn refactor_rename(
    sup: &crate::Supervisor,
    root: &Path,
    sym: &str,
    new_name: String,
) -> Result<Value, crate::ToolError> {
    let mut ctx = RecipeCtx::new(None, Some(sym.into()));
    let sym_s = ctx.sym.clone().expect("constructor sets sym");
    let impact = match ctx
        .read_step("ct_impact", crate::ct::ct_impact(sup, root, &sym_s))
        .await
    {
        Ok(v) => v,
        Err(e) => return Err(ctx.fail("refactor-rename", root, "ct_impact", e).await),
    };
    let def = crate::ct::def_hit(sup, root, &sym_s).await?;
    let (line, col) = crate::ct::name_position(sup, root, sym, &def).await;
    let (f2, nn) = (def.file.clone(), new_name.clone());
    if let Err(e) = ctx
        .write_step("rename-workspace", root, async move {
            let report = sup
                .tool_rename_symbol(root, &f2, line - 1, col - 1, &nn, None)
                .await?;
            Ok(report_to_env(&report))
        })
        .await
    {
        return Err(ctx.fail("refactor-rename", root, "rename-workspace", e).await);
    }
    let txn = ctx.txn_ids.last().copied();
    let after = match ctx
        .read_step("ct_verify_after", crate::ct::ct_verify(sup, root, &def.file, txn))
        .await
    {
        Ok(v) => v,
        Err(e) => return Err(ctx.fail("refactor-rename", root, "ct_verify_after", e).await),
    };
    Ok(json!({
        "recipe": "refactor-rename",
        "symbol": sym_s,
        "new_name": new_name,
        "definition_file": def.file,
        "txn_id": txn,
        "impact": impact,
        "verify_after": after,
        "steps": into_steps(&ctx),
    }))
}

/// review-diff `[txn-id]`：ct_review_diff 单步转发（读步）。
async fn review_diff(
    sup: &crate::Supervisor,
    root: &Path,
    txn_id: Option<u64>,
) -> Result<Value, crate::ToolError> {
    let mut ctx = RecipeCtx::new(None, None);
    let report = match ctx
        .read_step("ct_review_diff", crate::ct::ct_review_diff(sup, root, txn_id))
        .await
    {
        Ok(v) => v,
        Err(e) => return Err(ctx.fail("review-diff", root, "ct_review_diff", e).await),
    };
    Ok(json!({
        "recipe": "review-diff",
        "txn_id": txn_id,
        "report": report,
        "steps": into_steps(&ctx),
    }))
}

/// explore `<path>`：ct_tldr → repo-map → ct_recent_activity（三读步）。
async fn explore(
    sup: &crate::Supervisor,
    root: &Path,
    path: &str,
) -> Result<Value, crate::ToolError> {
    let mut ctx = RecipeCtx::new(Some(path.into()), None);
    let p = ctx.file.clone().expect("constructor sets file");
    let tldr = match ctx
        .read_step("ct_tldr", crate::ct::ct_tldr(sup, root, &p))
        .await
    {
        Ok(v) => v,
        Err(e) => return Err(ctx.fail("explore", root, "ct_tldr", e).await),
    };
    let repo_map = match ctx
        .read_step("repo-map", async {
            let report = crate::repo_map::build(sup, root, None, 40).await;
            Ok(json!({
                "total_symbols": report.total_symbols,
                "top": report.top,
            }))
        })
        .await
    {
        Ok(v) => v,
        Err(e) => return Err(ctx.fail("explore", root, "repo-map", e).await),
    };
    let recent = match ctx
        .read_step("ct_recent_activity", crate::ct::ct_recent_activity(root, 10))
        .await
    {
        Ok(v) => v,
        Err(e) => return Err(ctx.fail("explore", root, "ct_recent_activity", e).await),
    };
    Ok(json!({
        "recipe": "explore",
        "path": p,
        "tldr": tldr,
        "repo_map": repo_map,
        "recent_activity": recent,
        "steps": into_steps(&ctx),
    }))
}

// ============ 写步原语 ============

/// stub 落盘：已存在 → EOF append（insert-at-line total+1）；不存在 →
/// create-text-file。仅 .rs 目标（同 ct_define_feature stub 门）。写门 +
/// didChange + recorded_write 内建（进当前写步事务）。
async fn append_or_create(
    sup: &crate::Supervisor,
    root: &Path,
    file: &str,
    content: &str,
) -> Result<(), crate::ToolError> {
    if !file.ends_with(".rs") {
        return Err(crate::ToolError::BadArgs {
            detail: format!("stub generation supports .rs targets only, got {file}"),
        });
    }
    let abs = crate::path_guard::guarded_join(root, file)
        .map_err(|detail| crate::ToolError::BadArgs { detail })?;
    if abs.exists() {
        let existing = tokio::fs::read_to_string(&abs)
            .await
            .map_err(|e| crate::ToolError::BadArgs {
                detail: format!("read {file}: {e}"),
            })?;
        let sep = if existing.ends_with('\n') { "" } else { "\n" };
        let lines = existing.lines().count() as u32;
        sup.tool_insert_at_line(root, file, lines + 1, &format!("{sep}{content}"), None, None)
            .await
            .map(|_| ())
    } else {
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent).map_err(|e| crate::ToolError::BadArgs {
                detail: format!("mkdir {}: {e}", parent.display()),
            })?;
        }
        sup.tool_create_text_file(root, file, content, None).await.map(|_| ())
    }
}

/// EOF 追加（add-test 模板；文件必存在——定义文件已校验）。
async fn append_at_eof(
    sup: &crate::Supervisor,
    root: &Path,
    file: &str,
    content: &str,
) -> Result<(), crate::ToolError> {
    let abs = crate::path_guard::guarded_join(root, file)
        .map_err(|detail| crate::ToolError::BadArgs { detail })?;
    let existing = tokio::fs::read_to_string(&abs)
        .await
        .map_err(|e| crate::ToolError::BadArgs {
            detail: format!("read {file}: {e}"),
        })?;
    let sep = if existing.ends_with('\n') { "" } else { "\n" };
    let lines = existing.lines().count() as u32;
    sup.tool_insert_at_line(root, file, lines + 1, &format!("{sep}{content}"), None, None)
        .await
        .map(|_| ())
}

fn into_steps(ctx: &RecipeCtx) -> Value {
    Value::Array(
        ctx.findings
            .iter()
            .map(|(n, v)| json!({"step": n, "result": v}))
            .collect(),
    )
}

fn report_to_env(r: &crate::RenameReport) -> Value {
    json!({
        "files_modified": r.files_modified,
        "edits_applied": r.edits_applied,
        "files": r.files,
        "skipped": r.skipped,
    })
}

/// 单文件 LSP rename（计划 §2 批4 rename 规格）：定位名字 token →
/// prepareRename/rename → WorkspaceEdit 过滤到 `file` 所在文件（跨文件项计入
/// skipped，93q 先例），仅应用单文件 edits（同一写步事务）。
/// 前置：ct_impact 已跑通语义查询（语义索引已热，免 index-wait）。
async fn rename_in_file(
    sup: &crate::Supervisor,
    root: &Path,
    file: &str,
    symbol: &str,
    new_name: &str,
) -> Result<Value, crate::ToolError> {
    let hits = sup.tool_overview(root, file, None).await?;
    let Some(h) = hits.iter().find(|h| h.name == symbol) else {
        return Err(crate::ToolError::BadArgs {
            detail: format!("symbol `{symbol}` not found in {file}"),
        });
    };
    let def = crate::ct::DefHit {
        file: file.to_string(),
        line0: h.range.start.line,
        col0: h.range.start.character,
    };
    let (line, col) = crate::ct::name_position(sup, root, symbol, &def).await;
    let lang = crate::resolve_lang_for_file(file, None)?;
    let session = sup.session_for(root, &lang).await?;
    let path = crate::path_guard::guarded_join(root, file)
        .map_err(|detail| crate::ToolError::BadArgs { detail })?;
    let uri_str = path_to_uri_str(&path);
    let _guard = session.ensure_open(&path).await.map_err(crate::ToolError::Core)?;
    let _gate = crate::write_gate::acquire("recipe-rename").await?;
    let pos = crate::lsp_position_from_byte(&path, file, line - 1, col - 1, OffsetEncoding::Utf16).await?;
    let params = json!({
        "textDocument": { "uri": uri_str },
        "position": { "line": pos.line, "character": pos.character },
    });
    let prep: Option<Value> = session
        .request("textDocument/prepareRename", params.clone(), crate::TOOL_TIMEOUT)
        .await?;
    if prep.is_none() || prep.as_ref().is_some_and(Value::is_null) {
        return Err(crate::ToolError::BadArgs {
            detail: "prepareRename rejected this position".into(),
        });
    }
    let resp: Option<Value> = session
        .request(
            "textDocument/rename",
            json!({ "textDocument": { "uri": uri_str.clone() }, "position": { "line": pos.line, "character": pos.character }, "newName": new_name }),
            crate::TOOL_TIMEOUT,
        )
        .await?;
    let resp = resp.ok_or_else(|| crate::ToolError::Protocol {
        tool: "recipe:rename".into(),
        reason: "rename returned null".into(),
    })?;
    let by_uri = crate::parse_workspace_edit(&resp).ok_or_else(|| crate::ToolError::Protocol {
        tool: "recipe:rename".into(),
        reason: "rename response has neither `changes` map nor `documentChanges`".into(),
    })?;
    let root_canon = dunce::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let want_abs = dunce::canonicalize(&path).unwrap_or_else(|_| path.clone());
    let mut report = json!({
        "files_modified": 0,
        "edits_applied": 0,
        "files": [],
        "skipped": [],
    });
    for (uri, mut edits) in by_uri {
        edits.sort_by_key(|e| std::cmp::Reverse(e.0));
        let abs = match crate::uri_to_path(&uri) {
            Some(p) => p,
            None => {
                push_skipped(&mut report, &uri, "uri not resolvable");
                continue;
            }
        };
        if abs != want_abs {
            // 计划 rename 规格：跨文件 edits 不应用，计入 skipped 清单报告。
            let rel = crate::recipe::rel_forward(root, &abs.to_string_lossy());
            push_skipped(&mut report, &rel, "cross-file edit filtered by recipe rename (single-file scope)");
            continue;
        }
        let (abs, content, new_content) =
            match crate::prepare_rename_file(&root_canon, &uri, &edits).await {
                Ok(v) => v,
                Err(skip) => {
                    push_skipped(&mut report, &skip.file, &skip.reason);
                    continue;
                }
            };
        if new_content == content {
            continue;
        }
        crate::undo::recorded_write(&abs, &new_content)
            .await
            .map_err(|e| crate::ToolError::WriteConflict {
                path: abs.display().to_string(),
                reason: format!("atomic write failed: {e}"),
            })?;
        // 全量 didChange（ensure_open mtime 检测路径），content_version 单调。
        let _refreshed = session.ensure_open(&abs).await.map_err(crate::ToolError::Core)?;
        report["files_modified"] = json!(report["files_modified"].as_u64().unwrap_or(0) + 1);
        report["edits_applied"] =
            json!(report["edits_applied"].as_u64().unwrap_or(0) + edits.len() as u64);
        if let Some(files) = report.get_mut("files").and_then(Value::as_array_mut) {
            files.push(Value::String(crate::recipe::rel_forward(
                root,
                &abs.to_string_lossy(),
            )));
        }
    }
    Ok(report)
}

fn push_skipped(report: &mut Value, file: &str, reason: &str) {
    if let Some(skipped) = report.get_mut("skipped").and_then(Value::as_array_mut) {
        skipped.push(json!({"file": file, "reason": reason}));
    }
}

#[cfg(test)]
mod tests;
