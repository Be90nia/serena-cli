//! recipe 批1 地基三原子命令（local/recipe-plan.md §2 批1）：
//!
//! - `test <file> [name]`：cargo/npm 双后端包装（按路径扩展名/项目清单选后端；
//!   go 等后端留扩展点）。**产物不进 undo/txn 栈**——不调 `recorded_write`、
//!   不入 `undo::WRITE_TOOLS`（计划 §1.7 只读源码约束）。
//! - `diff [txn-id]`：读 undo store manifest 的写前写后对照；`--patch` 输出
//!   unified diff（`patch -p1` / `git apply` 可直接消费）。
//! - `find-test <sym>`：启发式链 tests/ 镜像路径 → 测试目录/命名文件内容引用 →
//!   `super::sym` 单测惯例 → LS refs 兜底过滤测试文件命中。
//!
//! 预算机制（计划 §1.2）：各工具内部自限 `*_BUDGET_TOKENS` 常量（4 bytes ≈ 1
//! token，与 §10-G 同近似），超限写 `truncated:true` + `original_count`（沿用
//! §10-G 词汇）；envelope 管线不管树形截断。
//!
//! ponytail: LCS 行 diff 不引第三方依赖（ARCH §8）；>1.5M cells 回退整文件替换单
//! hunk，patch 语义仍合法，只是粒度粗。

use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};

use crate::ToolError;
use lsp_core::types::SymbolHit;

/// `test` 输出预算（tokens，计划 §3 数值表）。
pub const TEST_BUDGET_TOKENS: usize = 2000;
/// `diff` 输出预算（tokens）。
pub const DIFF_BUDGET_TOKENS: usize = 2000;
/// `find-test` 输出预算（tokens）。
pub const FIND_TEST_BUDGET_TOKENS: usize = 400;

/// cargo/npm 子进程超时：冷构建（workspace 依赖编译）可到分钟级，取宽上限；
/// 超时映射既有 `CoreError::Timeout`（wire 零新错误码）。
const TEST_TIMEOUT_SECS: u64 = 600;
/// 单条失败消息长度上限（预算卫生）。
const MSG_MAX_CHARS: usize = 300;
/// LCS DP cells 上限，超出回退整文件替换单 hunk。
const DIFF_DP_MAX_CELLS: usize = 1_500_000;
/// unified diff 上下文行数（git 默认 3）。
const DIFF_CONTEXT: usize = 3;
/// find-test 文本搜索的命中保险丝（超限截断，测试命中可能被挤出——启发式工具可接受）。
const FIND_TEST_SEARCH_LIMIT: usize = 500;

// ============ test：后端选择 + 子进程包装 ============

/// 选定的测试后端与调用参数。
#[derive(Debug)]
enum TestTarget {
    Cargo { dir: std::path::PathBuf, args: Vec<String> },
    Npm { dir: std::path::PathBuf, args: Vec<String> },
}

impl TestTarget {
    fn backend_name(&self) -> &'static str {
        match self {
            TestTarget::Cargo { .. } => "cargo",
            TestTarget::Npm { .. } => "npm",
        }
    }
}

/// `test <file> [name]`：跑测试并解析输出。file = crate/test 目录或测试文件。
pub async fn run_test(root: &Path, file: &str, name: Option<&str>) -> Result<Value, ToolError> {
    let target = resolve_target(root, file, name)?;
    let output = run_backend(&target).await?;
    let (failures, passed, failed) = match target.backend_name() {
        "cargo" => parse_rust_output(&output.0),
        _ => parse_npm_output(&output.0),
    };
    let mut resp = json!({
        "backend": target.backend_name(),
        "passed": passed,
        "failed": failed,
        "raw_exit": output.1,
        "failures": failures,
    });
    truncate_list_field(&mut resp, "failures", TEST_BUDGET_TOKENS * 4);
    Ok(resp)
}

/// 按目标形态选后端：目录看项目清单（Cargo.toml / package.json），文件看扩展名。
/// go 等其他后端在此扩展（计划：只有 cargo+npm，go 留扩展点）。
fn resolve_target(root: &Path, file: &str, name: Option<&str>) -> Result<TestTarget, ToolError> {
    let p = root.join(file);
    let meta = std::fs::metadata(&p).map_err(|_| ToolError::BadArgs {
        detail: format!("test target not found: {}", p.display()),
    })?;
    let name_args: Vec<String> = name
        .filter(|n| !n.is_empty())
        .map(|n| vec![n.to_string()])
        .unwrap_or_default();
    if meta.is_dir() {
        if p.join("Cargo.toml").is_file() {
            let mut args = vec!["test".to_string()];
            args.extend(name_args);
            return Ok(TestTarget::Cargo { dir: p, args });
        }
        if p.join("package.json").is_file() {
            return Ok(TestTarget::Npm { dir: p, args: npm_args(Vec::new(), name_args) });
        }
        return Err(ToolError::BadArgs {
            detail: format!("directory has no Cargo.toml or package.json: {}", p.display()),
        });
    }
    let ext = p
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    match ext.as_deref() {
        Some("rs") => {
            let dir = ancestor_with(&p, "Cargo.toml")?;
            let mut args = vec!["test".to_string()];
            // tests/<stem>.rs 集成测试 → --test 精准化到目标，避免全仓跑。
            if p.parent()
                .and_then(|d| d.file_name())
                .is_some_and(|d| d == "tests")
                && let Some(stem) = p.file_stem()
            {
                args.push("--test".into());
                args.push(stem.to_string_lossy().to_string());
            }
            args.extend(name_args);
            Ok(TestTarget::Cargo { dir, args })
        }
        Some("js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs") => {
            let dir = ancestor_with(&p, "package.json")?;
            let extra = vec![p.to_string_lossy().replace('\\', "/")];
            Ok(TestTarget::Npm { dir, args: npm_args(extra, name_args) })
        }
        other => Err(ToolError::BadArgs {
            detail: format!(
                "unsupported test file type {other:?}; supported backends: cargo (.rs), npm (.js/.jsx/.ts/.tsx/.mjs/.cjs)"
            ),
        }),
    }
}

/// npm test 透传参数：有附加模式（文件路径/名字过滤）才加 `--` 分隔（npm run 约定）。
fn npm_args(extra: Vec<String>, name_args: Vec<String>) -> Vec<String> {
    let mut args = vec!["test".to_string()];
    if !extra.is_empty() || !name_args.is_empty() {
        args.push("--".into());
        args.extend(extra);
        args.extend(name_args);
    }
    args
}

/// 自下而上找含 marker 的最近祖先目录（项目根）。
fn ancestor_with(start: &Path, marker: &str) -> Result<std::path::PathBuf, ToolError> {
    let mut cur = Some(start);
    while let Some(d) = cur {
        if d.join(marker).is_file() {
            return Ok(d.to_path_buf());
        }
        cur = d.parent();
    }
    Err(ToolError::BadArgs {
        detail: format!("no {marker} found in any ancestor of {}", start.display()),
    })
}

/// 跑后端命令，返回 (合并输出, 退出码)。超时 kill 子进程（kill_on_drop）。
async fn run_backend(t: &TestTarget) -> Result<(String, i32), ToolError> {
    let (dir, args) = match t {
        TestTarget::Cargo { dir, args } => (dir, args),
        TestTarget::Npm { dir, args } => (dir, args),
    };
    let mut cmd = tokio::process::Command::new(backend_program(t));
    if let TestTarget::Npm { .. } = t {
        // Windows npm 是 .cmd shim，裸 spawn 找不到；cmd /C 走 PATHEXT 解析。
        cmd.arg("/C");
    }
    cmd.args(args)
        .current_dir(dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let out = tokio::time::timeout(Duration::from_secs(TEST_TIMEOUT_SECS), cmd.output())
        .await
        .map_err(|_| ToolError::Core(lsp_core::error::CoreError::Timeout {
            method: "test".into(),
            secs: TEST_TIMEOUT_SECS,
        }))?
        .map_err(|e| ToolError::Core(lsp_core::error::CoreError::Io(e)))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    Ok((text, out.status.code().unwrap_or(-1)))
}

/// Windows 上 npm 走 `cmd /C npm`（.cmd shim）；cargo 是原生 exe 直接 spawn。
fn backend_program(t: &TestTarget) -> String {
    match t {
        TestTarget::Cargo { .. } => "cargo".to_string(),
        TestTarget::Npm { .. } if cfg!(windows) => "cmd".to_string(),
        TestTarget::Npm { .. } => "npm".to_string(),
    }
}

// ============ test：输出解析器（独立交付物，计划 §1.7） ============

/// 解析产出：failures[{file,line,col,msg,level}] + passed/failed 计数。
type Parsed = (Vec<Value>, usize, usize);

/// rustc/cargo 后端：`--> file:line:col` 诊断块（error/warning）+ `test result:`
/// 汇总计数 + `panicked at file:line:col` 失败定位。无 `-->` 的头部（如
/// "error: could not compile"）无位置语义，按规格丢弃。passed/failed 只取
/// `test result:` 行（编译期失败 = 0/0 + failures 清单，诚实口径）。
fn parse_rust_output(text: &str) -> Parsed {
    let mut failures = Vec::new();
    let (mut passed, mut failed) = (0usize, 0usize);
    // pending 诊断头：一个头可挂多个 --> span（主次位置），保持到下个头覆盖。
    let mut pending: Option<(&'static str, String)> = None;
    for line in text.lines() {
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix("-->") {
            if let Some((f, l, c)) = parse_rustc_loc(rest.trim()) {
                let (level, msg) = match &pending {
                    Some((lv, m)) => (*lv, m.clone()),
                    None => ("error", "error".to_string()),
                };
                failures.push(json!({
                    "file": f, "line": l, "col": c,
                    "msg": clamp_msg(&msg), "level": level,
                }));
            }
            continue;
        }
        if t.starts_with("error") {
            pending = Some(("error", diag_msg(t)));
            continue;
        }
        if t.starts_with("warning") {
            pending = Some(("warning", diag_msg(t)));
            continue;
        }
        if let Some(rest) = t.strip_prefix("test result:") {
            for part in rest.split(';') {
                if let Some(n) = count_before(part, "passed") {
                    passed += n;
                }
                if let Some(n) = count_before(part, "failed") {
                    failed += n;
                }
            }
            continue;
        }
        // Rust ≥1.73 panic 定位：thread 'tests::x' panicked at src/lib.rs:12:5:
        if let Some(rest) = t.strip_prefix("thread ")
            && let Some(at) = rest.find("panicked at ")
        {
            let thread = rest[1..].split('\'').next().unwrap_or("?");
            let loc = rest[at + "panicked at ".len()..].trim().trim_end_matches(':');
            if let Some((f, l, c)) = parse_rustc_loc(loc) {
                failures.push(json!({
                    "file": f, "line": l, "col": c,
                    "msg": clamp_msg(&format!("panicked in `{thread}`")), "level": "fail",
                }));
            }
        }
    }
    (failures, passed, failed)
}

/// npm/jest 后端：`✕` 失败块 + `at ... (file:line:col)` 项目内首栈帧 + `Tests:`
/// 汇总行。每个 ✕ 只取首个项目内栈帧（同一失败的多帧降噪）；node_modules 帧丢弃。
fn parse_npm_output(text: &str) -> Parsed {
    let mut failures = Vec::new();
    let (mut passed, mut failed) = (0usize, 0usize);
    let mut current_fail: Option<String> = None;
    for line in text.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix('✕') {
            current_fail = Some(strip_duration(rest.trim()));
            continue;
        }
        if t.starts_with("at ") && let Some(name) = current_fail.clone() {
            if let Some((f, l, c)) = parse_stack_frame(t)
                && !f.contains("node_modules")
            {
                failures.push(json!({
                    "file": f, "line": l, "col": c,
                    "msg": clamp_msg(&name), "level": "fail",
                }));
                current_fail = None;
            }
            continue;
        }
        if let Some(rest) = t.strip_prefix("Tests:") {
            for part in rest.split(',') {
                if let Some(n) = count_before(part, "failed") {
                    failed += n;
                } else if let Some(n) = count_before(part, "passed") {
                    passed += n;
                }
            }
        }
    }
    (failures, passed, failed)
}

/// `error[E0308]: msg` / `warning: msg` → 位置语义消息（首个 ": " 之后；无则原行）。
fn diag_msg(t: &str) -> String {
    match t.split_once(": ") {
        Some((_, m)) => m.to_string(),
        None => t.to_string(),
    }
}

/// `file:line:col` 解析（从尾部取 3 段，容忍 Windows 盘符冒号）；`\` → `/`。
fn parse_rustc_loc(s: &str) -> Option<(String, u32, u32)> {
    let s = s.trim_end_matches(':');
    let col_pos = s.rfind(':')?;
    let line_pos = s[..col_pos].rfind(':')?;
    let file = &s[..line_pos];
    let line: u32 = s[line_pos + 1..col_pos].parse().ok()?;
    let col: u32 = s[col_pos + 1..].parse().ok()?;
    if line == 0 || col == 0 {
        return None;
    }
    Some((file.replace('\\', "/"), line, col))
}

/// JS 栈帧 `at ... (file:line:col)` → 位置。
fn parse_stack_frame(t: &str) -> Option<(String, u32, u32)> {
    let inner = t.rsplit_once('(')?.1.strip_suffix(')')?;
    parse_rustc_loc(inner.trim())
}

/// jest `✕ name (5 ms)` → 剥尾部时长。
fn strip_duration(name: &str) -> String {
    match name.rsplit_once(" (") {
        Some((n, _)) if name.ends_with(')') => n.to_string(),
        _ => name.to_string(),
    }
}

/// `"3 passed"` → 3；无数字前缀 → None。
fn count_before(part: &str, word: &str) -> Option<usize> {
    let idx = part.find(word)?;
    let digits = part[..idx].trim().rsplit(' ').next()?;
    digits.parse().ok()
}

fn clamp_msg(s: &str) -> String {
    match s.char_indices().nth(MSG_MAX_CHARS) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

// ============ diff：事务写前写后对照 ============

/// `diff [txn-id] [--patch]`：读 undo store 单事务快照，输出逐文件增删统计；
/// `--patch` 附 unified diff（patch -p1 / git apply 可消费）。
pub async fn diff_txn(
    root: &Path,
    txn_id: Option<u64>,
    patch: bool,
) -> Result<Value, ToolError> {
    let snap = crate::undo::read_txn(root, txn_id).await?;
    let mut files = Vec::with_capacity(snap.files.len());
    let mut patch_buf = String::new();
    for f in &snap.files {
        let rel = rel_forward(root, &f.path);
        let before = f.before.as_deref().unwrap_or("");
        let after = f.after.as_deref().unwrap_or("");
        let d = unified_diff(&rel, before, after, f.created);
        let (added, removed) = diff_line_stats(&d);
        files.push(json!({
            "path": rel, "created": f.created,
            "added": added, "removed": removed,
        }));
        if patch {
            patch_buf.push_str(&d);
        }
    }
    let mut resp = json!({
        "txn_id": snap.txn_id,
        "timestamp": snap.timestamp,
        "files": files,
    });
    let budget = DIFF_BUDGET_TOKENS * 4;
    if patch {
        resp["patch"] = Value::String(patch_buf.clone());
        // patch 行为预算主体：整行截断（断行 patch 不可消费），original_count =
        // 截断前 diff 总行数。
        if patch_buf.len() > budget {
            let total = patch_buf.lines().count();
            let keep = max_fit_lines(&patch_buf, budget);
            resp["patch"] = Value::String(patch_buf.lines().take(keep).collect::<Vec<_>>().join("\n"));
            resp["truncated"] = json!(true);
            resp["original_count"] = json!(total);
        }
    }
    // patch 截断后仍超（病态多文件事务）→ 再截 files 摘要（覆盖计数，最后手段）。
    truncate_list_field(&mut resp, "files", budget);
    Ok(resp)
}

/// 绝对路径 → 相对 root 的正斜杠路径（root 外文件保持绝对）。
pub(crate) fn rel_forward(root: &Path, abs: &str) -> String {
    Path::new(abs)
        .strip_prefix(root)
        .unwrap_or(Path::new(abs))
        .to_string_lossy()
        .replace('\\', "/")
}

/// unified diff 中 +/- 行计数（排除 ---/+++ 头）。
fn diff_line_stats(diff: &str) -> (usize, usize) {
    let (mut added, mut removed) = (0usize, 0usize);
    for l in diff.lines() {
        if l.starts_with('+') && !l.starts_with("+++") {
            added += 1;
        } else if l.starts_with('-') && !l.starts_with("---") {
            removed += 1;
        }
    }
    (added, removed)
}

/// 预算内最多保留的整行数（二分；keep ≥1 保底可消费）。
fn max_fit_lines(text: &str, budget_bytes: usize) -> usize {
    let lines: Vec<&str> = text.lines().collect();
    let (mut lo, mut hi) = (1usize, lines.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let bytes = lines[..mid].join("\n").len();
        if bytes + 64 <= budget_bytes {
            lo = mid;
        } else if mid == 1 {
            break;
        } else {
            hi = mid - 1;
        }
    }
    lo
}

// ============ unified diff 生成（LCS 行 diff，无第三方依赖） ============

/// 行切分：(行数组, 末行是否以 \n 收尾)。空串 = 0 行 + 收尾 true（新建/清空语义）。
fn split_lines(s: &str) -> (Vec<&str>, bool) {
    if s.is_empty() {
        return (Vec::new(), true);
    }
    let ends_nl = s.ends_with('\n');
    let body = if ends_nl { &s[..s.len() - 1] } else { s };
    (body.split('\n').collect(), ends_nl)
}

/// 单文件 unified diff。created → 旧头 `/dev/null`（git 新文件约定）。
/// 无差异（内容与收尾换行都一致）→ 空串。
pub(crate) fn unified_diff(rel: &str, before: &str, after: &str, created: bool) -> String {
    let (a, a_nl) = split_lines(before);
    let (b, b_nl) = split_lines(after);
    if a == b && a_nl == b_nl {
        return String::new();
    }
    let mut out = String::new();
    if created {
        out.push_str("--- /dev/null\n");
    } else {
        out.push_str(&format!("--- a/{rel}\n"));
    }
    out.push_str(&format!("+++ b/{rel}\n"));
    let ops = if a.len().saturating_mul(b.len()) > DIFF_DP_MAX_CELLS {
        // 超大文件免 LCS：整文件替换单 hunk（合法 unified，粒度粗）。
        whole_replace(a.len(), b.len())
    } else {
        lcs_ops(&a, &b)
    };
    // 仅行尾换行差异（无增删行）→ LCS 无变更点，hunk 生成器产不出东西；
    // 退整体替换，保证 patch 仍可消费。
    let ops = if ops.iter().all(|op| matches!(op, Op::Keep(_))) {
        whole_replace(a.len(), b.len())
    } else {
        ops
    };
    emit_hunks(&mut out, &a, a_nl, &b, b_nl, &ops);
    out
}

fn whole_replace(n_a: usize, n_b: usize) -> Vec<Op> {
    (0..n_a).map(Op::Del).chain((0..n_b).map(Op::Add)).collect()
}

enum Op {
    Keep(usize),
    Del(usize),
    Add(usize),
}

/// LCS 编辑脚本（对齐 a[i]==b[j] 的最长公共子序列）。
fn lcs_ops(a: &[&str], b: &[&str]) -> Vec<Op> {
    let (n, m) = (a.len(), b.len());
    let w = m + 1;
    let mut dp = vec![0u32; (n + 1) * w];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i * w + j] = if a[i] == b[j] {
                dp[(i + 1) * w + j + 1] + 1
            } else {
                dp[(i + 1) * w + j].max(dp[i * w + j + 1])
            };
        }
    }
    let mut ops = Vec::with_capacity(n + m);
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if a[i] == b[j] {
            ops.push(Op::Keep(i));
            i += 1;
            j += 1;
        } else if dp[(i + 1) * w + j] >= dp[i * w + j + 1] {
            ops.push(Op::Del(i));
            i += 1;
        } else {
            ops.push(Op::Add(j));
            j += 1;
        }
    }
    ops.extend((i..n).map(Op::Del));
    ops.extend((j..m).map(Op::Add));
    ops
}

/// 按 git 约定输出 hunk：3 行上下文、keep-run > 2×context 断组、
/// 空范围起点 0、末行无换行发 "\ No newline at end of file"。
fn emit_hunks(out: &mut String, a: &[&str], a_nl: bool, b: &[&str], b_nl: bool, ops: &[Op]) {
    let changes: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter(|(_, op)| !matches!(op, Op::Keep(_)))
        .map(|(i, _)| i)
        .collect();
    if changes.is_empty() {
        return; // 仅行尾换行差异——极罕见；不产 hunk（ponytail：可接受近似）
    }
    let mut groups: Vec<(usize, usize)> = Vec::new();
    let (mut gs, mut ge) = (changes[0], changes[0]);
    for &c in &changes[1..] {
        if c - ge - 1 > 2 * DIFF_CONTEXT {
            groups.push((gs, ge));
            gs = c;
        }
        ge = c;
    }
    groups.push((gs, ge));

    for (gs, ge) in groups {
        let start = gs.saturating_sub(DIFF_CONTEXT);
        let end = (ge + 1 + DIFF_CONTEXT).min(ops.len());
        // 头坐标：ctx 窗口前的 a/b 已消耗行数（0-based → 输出 1-based）。
        let mut a_pos = 0usize;
        let mut b_pos = 0usize;
        for op in &ops[..start] {
            match op {
                Op::Add(_) => b_pos += 1,
                Op::Del(_) => a_pos += 1,
                Op::Keep(_) => {
                    a_pos += 1;
                    b_pos += 1;
                }
            }
        }
        let mut a_len = 0usize;
        let mut b_len = 0usize;
        let mut body = String::new();
        for op in &ops[start..end] {
            match op {
                Op::Keep(i) => {
                    a_len += 1;
                    b_len += 1;
                    body.push(' ');
                    body.push_str(a[*i]);
                    body.push('\n');
                }
                Op::Del(i) => {
                    a_len += 1;
                    body.push('-');
                    body.push_str(a[*i]);
                    body.push('\n');
                    if *i + 1 == a.len() && !a_nl {
                        body.push_str("\\ No newline at end of file\n");
                    }
                }
                Op::Add(j) => {
                    b_len += 1;
                    body.push('+');
                    body.push_str(b[*j]);
                    body.push('\n');
                    if *j + 1 == b.len() && !b_nl {
                        body.push_str("\\ No newline at end of file\n");
                    }
                }
            }
        }
        // git 约定：空范围起点写 0（新文件 `@@ -0,0 +1,N @@`）。
        let a_start = if a_len == 0 { a_pos } else { a_pos + 1 };
        let b_start = if b_len == 0 { b_pos } else { b_pos + 1 };
        out.push_str(&format!("@@ -{a_start},{a_len} +{b_start},{b_len} @@\n"));
        out.push_str(&body);
    }
}

// ============ find-test：启发式链 ============

/// `find-test <sym>`：H1 tests/ 镜像探针（fs，免 LS）→ H2/H3 文本搜索过滤测试
/// 路径与 `super::` 单测惯例（免 LS）→ H4 LS refs 兜底（仅 H1-H3 全空时跑，
/// best-effort：失败显式记 `ls_refs_skipped`，不静默）。
pub async fn find_test(sup: &crate::Supervisor, root: &Path, symbol: &str) -> Result<Value, ToolError> {
    if symbol.is_empty() {
        return Err(ToolError::BadArgs {
            detail: "symbol must not be empty".into(),
        });
    }
    let mut hits: Vec<Value> = Vec::new();
    // H1：tests/ 镜像路径探针。
    for rel in mirror_probes(root, symbol) {
        push_hit(&mut hits, &rel, 1, 1, "mirror");
    }
    // H2/H3：全仓单词匹配 → 测试路径 / 命名约定 / super:: 单测惯例。
    let resp = sup.tool_search_for_pattern(root, &format!(r"\b{}\b", regex::escape(symbol)), None, FIND_TEST_SEARCH_LIMIT, true, &[], false)
        .await?;
    let sym_lower = symbol.to_ascii_lowercase();
    for h in &resp.hits {
        let has_super = h.text.contains("super::");
        if !is_test_path(&h.file) && !has_super {
            continue;
        }
        let stem = h
            .file
            .rsplit('/')
            .next()
            .and_then(|f| f.rsplit_once('.'))
            .map(|(s, _)| s.to_ascii_lowercase())
            .unwrap_or_default();
        let source = if stem.contains(&sym_lower) {
            "name_pattern"
        } else if has_super {
            "mod_tests"
        } else {
            "content_ref"
        };
        push_hit(&mut hits, &h.file, h.line, h.col, source);
    }
    // H4：LS refs 兜底（best-effort，失败显式带出）。
    let mut ls_refs_skipped: Option<String> = None;
    if hits.is_empty() {
        match sup.tool_find_symbol(root, symbol, 20, None).await {
            Ok((items, _)) => {
                if let Some(hit) = items.iter().find(|i| i.name == symbol) {
                    ls_refs_skipped = refs_to_test_hits(sup, root, hit, &mut hits).await;
                } else {
                    ls_refs_skipped = Some(format!("ls_refs: symbol `{symbol}` not found by workspace/symbol"));
                }
            }
            Err(e) => ls_refs_skipped = Some(format!("ls_refs skipped: {e}")),
        }
    }
    let mut out = json!({ "symbol": symbol, "hits": hits });
    if let Some(note) = ls_refs_skipped {
        out["ls_refs_skipped"] = json!(note);
    }
    // hits 键与 §10-G BUDGET_LIST_KEYS 对齐 → 直接复用 apply_budget 截断。
    crate::apply_budget(&mut out, FIND_TEST_BUDGET_TOKENS);
    Ok(out)
}

/// H4：对符号定义位置拉 refs，过滤测试文件命中。返回 None = 成功（或无可记录
/// 失败）；Some(原因) = 兜底未跑成/未命中的显式说明。
async fn refs_to_test_hits(
    sup: &crate::Supervisor,
    root: &Path,
    hit: &SymbolHit,
    hits: &mut Vec<Value>,
) -> Option<String> {
    let Some(path) = crate::uri_to_path(&hit.uri) else {
        return Some("ls_refs: non-file uri".into());
    };
    let rel = path
        .strip_prefix(root)
        .unwrap_or(&path)
        .to_string_lossy()
        .replace('\\', "/");
    // SymbolHit.range 与 tool_refs 同为 LSP 0-based 原样透传（A3b #2 同款修正）。
    let refs = sup
        .tool_refs(
            root,
            &rel,
            hit.range.start.line,
            hit.range.start.character,
            None,
        )
        .await;
    match refs {
        Ok(locs) => {
            for loc in locs {
                if let Some(p) = crate::uri_to_path(loc.uri.as_str()) {
                    let rel = p
                        .strip_prefix(root)
                        .unwrap_or(&p)
                        .to_string_lossy()
                        .replace('\\', "/");
                    if is_test_path(&rel) {
                        push_hit(
                            hits,
                            &rel,
                            loc.range.start.line + 1,
                            loc.range.start.character + 1,
                            "ls_refs",
                        );
                    }
                }
            }
            None
        }
        Err(e) => Some(format!("ls_refs failed: {e}")),
    }
}

/// H1 探针表：tests|test|__tests__ 目录 × rust/js 命名模板。
fn mirror_probes(root: &Path, symbol: &str) -> Vec<String> {
    let mut out = Vec::new();
    let rs = [
        format!("{symbol}.rs"),
        format!("{symbol}_test.rs"),
        format!("test_{symbol}.rs"),
        format!("{symbol}/mod.rs"),
    ];
    for dir in ["tests", "test", "__tests__"] {
        for name in &rs {
            let rel = format!("{dir}/{name}");
            if root.join(&rel).is_file() {
                out.push(rel);
            }
        }
        for ext in ["js", "jsx", "ts", "tsx", "mjs", "cjs"] {
            for name in [
                format!("{symbol}.test.{ext}"),
                format!("{symbol}.spec.{ext}"),
                format!("{symbol}_test.{ext}"),
            ] {
                let rel = format!("{dir}/{name}");
                if root.join(&rel).is_file() {
                    out.push(rel);
                }
            }
        }
    }
    out
}

/// 测试路径判定：tests/test/__tests__/spec 目录段，或 *_test/test_*/.*test/.spec 命名。
fn is_test_path(file: &str) -> bool {
    if let Some((stem, _)) = file.rsplit('/').next().and_then(|f| f.rsplit_once('.')) {
        let stem = stem.to_ascii_lowercase();
        if stem.ends_with("_test")
            || stem.starts_with("test_")
            || stem.ends_with(".test")
            || stem.ends_with(".spec")
        {
            return true;
        }
    }
    file.split('/').any(|seg| {
        matches!(
            seg,
            "tests" | "test" | "__tests__" | "spec" | "__specs__"
        )
    })
}

/// 按文件去重的命中追加（首个来源优先）。
fn push_hit(hits: &mut Vec<Value>, file: &str, line: u32, col: u32, source: &str) {
    if hits.iter().any(|h| h["file"] == file) {
        return;
    }
    hits.push(json!({ "file": file, "line": line, "col": col, "source": source }));
}

// ============ 预算截断（自定义 list 键；hits 走 crate::apply_budget） ============

/// 信封内指定 list 键的预算截断：二分保留条数，写 truncated/original_count
/// （§10-G 同词汇同语义；BUDGET_LIST_KEYS 之外的键名走这里）。
pub(crate) fn truncate_list_field(value: &mut Value, key: &str, budget_bytes: usize) -> bool {
    let current = serde_json::to_vec(value).map(|v| v.len()).unwrap_or(0);
    let Some(items) = value.get_mut(key).and_then(|v| v.as_array_mut()) else {
        return false;
    };
    if current <= budget_bytes {
        return false;
    }
    let original_count = items.len();
    let (mut lo, mut hi) = (0usize, items.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let trial = serde_json::to_vec(&json!({ key: &items[..mid] }))
            .map(|v| v.len())
            .unwrap_or(0);
        if trial + 18 <= budget_bytes {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    items.truncate(lo);
    if let Some(obj) = value.as_object_mut() {
        obj.insert("truncated".into(), json!(true));
        obj.insert("original_count".into(), json!(original_count));
    }
    true
}

#[cfg(test)]
mod tests;
