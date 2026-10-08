//! Task 24: 引用查询工具（find_referencing_symbols / find_referencing_code_snippets）。
//!
//! 设计要点（上游 solidlsp `find_referencing_*` 复刻 + 简化）：
//! - **入口统一**：`textDocument/references` 拿 `Location[]`；
//! - **find_referencing_symbols**：每个 ref 反查 `textDocument/documentSymbol` 找**外层
//!   容器**（类/方法/函数名），按 file + 容器聚类去重；
//! - **find_referencing_code_snippets**：每个 ref 取前后 N 行代码片段；
//! - **同 file 的多 ref 共享一次 documentSymbol / read_to_string**（N+1 → 1+1 调用）。
//!
//! ponylabel: 容器推断走 documentSymbol；clangd 对 100+ ref 文件 docSymbol 耗时 ~50ms。
//! ponytail: 升级到 LSP `callHierarchy`（需服务器能力声明，clangd 不支持）。

use std::collections::{HashMap, HashSet};
use std::path::Path;

use lsp_core::docsync::path_to_uri_str;
use lsp_core::session::Session;

use crate::uri_to_path;

use std::sync::Arc;

use lsp_types::{DocumentSymbol, DocumentSymbolResponse, Location, Position, Range};
use serde::Serialize;
use serde_json::json;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RefError {
    #[error("bad args: {detail}")]
    BadArgs { detail: String },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("core: {0}")]
    Core(String),
}

pub type RefResult<T> = std::result::Result<T, RefError>;

/// `find_referencing_symbols` 的单条结果（按 file + 容器聚类）。
#[derive(Debug, Serialize)]
pub struct RefSymbolHit {
    pub file: String,
    pub line: u32,
    pub col: u32,
    /// ref 落在哪个外层符号里（类/方法/函数名）。顶层时空字符串。
    pub container_name: String,
}

/// `find_referencing_code_snippets` 的单条结果。
#[derive(Debug, Serialize)]
pub struct RefSnippetHit {
    pub file: String,
    pub line: u32,
    pub col: u32,
    /// ref 处单行（trim 末尾换行）。
    pub text: String,
    /// ref 前后 N 行的代码片段（多行用 `\n` 分隔）。
    pub snippet: String,
}

async fn fetch_references(
    session: &Arc<Session>,

    file: &Path,
    line: u32,
    col: u32,
) -> RefResult<Vec<Location>> {
    let _guard = session
        .ensure_open(file)
        .await
        .map_err(|e| RefError::Core(format!("ensure_open: {e}")))?;
    // 跨文件引用查询发请求前的 `$/progress` 索引等待（didOpen 之后、request 之前，
    // 位次同上游基类 ReferencesLocationRequest.execute）。
    // ↖ mirror: ls.py@43ae021 `_wait_for_cross_file_references_if_needed`
    //           ↖ mirror: @cf54869a 修订 —— 后续查询也 drain 在飞索引。默认空实现，
    // 仅跟踪 $/progress 的 adapter（typescript-language-server）真正等待；失败/超时
    // 不阻断查询（hook 内部 warn 后放行）。
    if let Some(adapter) = ls_registry::adapter_for(&session.language_id()) {
        adapter.wait_for_cross_file_index(session).await;
        // references 请求前的适配器钩子（audit 竞锁 #2 接线点）：nextflow 的延迟
        // 工作区扫描 flush 在此真实触发——trait 钩子此前全仓零调用点（「机制存在≠
        // 接线生效」第三例），扫描窗口内 references 静默空。默认实现为空，其余
        // 语言零行为。
        adapter.pre_references(session, file).await;
    }
    let uri = path_to_uri_str(file);
    let params = json!({
        "textDocument": { "uri": uri },
        "position": { "line": line, "character": col },
        "context": { "includeDeclaration": true },
    });
    let raw: Option<serde_json::Value> = session
        .request(
            "textDocument/references",
            params,
            std::time::Duration::from_secs(30),
        )
        .await
        .map_err(|e| RefError::Core(format!("references: {e}")))?;
    Ok(crate::normalize_implementations(raw.as_ref()))
}

async fn fetch_document_symbols(
    session: &Arc<Session>,
    file: &Path,
) -> RefResult<Vec<DocumentSymbol>> {
    let _guard = session
        .ensure_open(file)
        .await
        .map_err(|e| RefError::Core(format!("ensure_open: {e}")))?;
    let uri = path_to_uri_str(file);
    let params = json!({ "textDocument": { "uri": uri } });
    let resp: Option<DocumentSymbolResponse> = session
        .request(
            "textDocument/documentSymbol",
            params,
            std::time::Duration::from_secs(30),
        )
        .await
        .map_err(|e| RefError::Core(format!("documentSymbol: {e}")))?;
    let mut out = Vec::new();
    match resp {
        Some(DocumentSymbolResponse::Nested(items)) => {
            flatten(&items, &mut out);
        }
        Some(DocumentSymbolResponse::Flat(items)) => {
            for it in items {
                out.push(DocumentSymbol {
                    name: it.name.clone(),
                    detail: None,
                    kind: it.kind,
                    tags: None,
                    #[allow(deprecated)]
                    deprecated: None,
                    range: it.location.range,
                    selection_range: it.location.range,
                    children: None,
                });
            }
        }
        None => {}
    }
    Ok(out)
}

fn flatten(items: &[DocumentSymbol], out: &mut Vec<DocumentSymbol>) {
    for it in items {
        out.push(DocumentSymbol {
            name: it.name.clone(),
            detail: it.detail.clone(),
            kind: it.kind,
            tags: it.tags.clone(),
            #[allow(deprecated)]
            deprecated: it.deprecated,
            range: it.range,

            selection_range: it.selection_range,
            children: None,
        });
        if let Some(children) = &it.children {
            flatten(children, out);
        }
    }
}

fn contains(r: Range, p: Position) -> bool {
    let after_start =
        p.line > r.start.line || (p.line == r.start.line && p.character >= r.start.character);
    let before_end =
        p.line < r.end.line || (p.line == r.end.line && p.character <= r.end.character);
    after_start && before_end
}

fn area(r: Range) -> u64 {
    let h = (r.end.line as i64 - r.start.line as i64).unsigned_abs();
    let w = if h == 0 {
        (r.end.character as i64 - r.start.character as i64).unsigned_abs()
    } else {
        r.end.character as u64
    };
    h.saturating_mul(w)
}

fn find_container_name(symbols: &[DocumentSymbol], line: u32, col: u32) -> String {
    let pos = Position::new(line, col);
    let mut best: Option<&DocumentSymbol> = None;
    for sym in symbols {
        if contains(sym.range, pos) {
            let replace = match best {
                None => true,
                Some(b) => area(sym.range) < area(b.range),
            };
            if replace {
                best = Some(sym);
            }
        }
    }
    best.map(|s| s.name.clone()).unwrap_or_default()
}

/// ts/js 系扩展名 → hybrid 伴生语义会话（见 `find_referencing_symbols` 头注释）。
/// 其余情况原会话返回 —— 无 hybrid 伴生 / 非 ts/js 文件时行为与改动前逐字节一致。
fn semantic_session_for_file(
    session: &std::sync::Arc<Session>,
    root: &Path,
    file: &str,
) -> std::sync::Arc<Session> {
    // per-file 重路由优先：angular `.html` references → ngserver 伴生（↖ mirror
    // 上游路由表——ngserver 聚合模板+TS 引用）；未路由走下方 ts/js 伴生判断。
    if let Some(adapter) = ls_registry::adapter_for(&session.language_id())
        && let Some(rerouted) =
            adapter.session_for_file(root, std::path::Path::new(file), "textDocument/references")
    {
        return rerouted;
    }
    let is_ts_like = std::path::Path::new(file)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .is_some_and(|e| {
            matches!(
                e.as_str(),
                "ts" | "tsx" | "mts" | "cts" | "js" | "jsx" | "mjs" | "cjs"
            )
        });
    if is_ts_like
        && let Some(adapter) = ls_registry::adapter_for(&session.language_id())
        && let Some(companion) = adapter.semantic_session(root)
    {
        return companion;
    }
    std::sync::Arc::clone(session)
}

/// hybrid 双服务器语言 `.宿主` 出发的双查合并是否适用：宿主语言是 astro 且目标是
/// `.astro`，或宿主是 svelte 且目标是 `.svelte`（ts/js 系文件已由
/// `semantic_session_for_file` 纯伴生路由覆盖，不进双查）。
fn hybrid_companion_applicable(language_id: &str, file: &Path) -> bool {
    match language_id {
        "astro" => file
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("astro")),
        "svelte" => file
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("svelte")),
        _ => false,
    }
}

/// 主+伴生两路 `Location` 按 (uri, line, col) 去重合并，主路优先。
/// ↖ mirror: astro_language_server.py@7a296833 `_deduplicate_reference_locations`。
fn merge_reference_locations(primary: Vec<Location>, companion: Vec<Location>) -> Vec<Location> {
    let mut seen = HashSet::with_capacity(primary.len() + companion.len());
    let mut out = Vec::with_capacity(primary.len() + companion.len());
    for loc in primary.into_iter().chain(companion) {
        let key = (
            loc.uri.as_str().to_string(),
            loc.range.start.line,
            loc.range.start.character,
        );
        if seen.insert(key) {
            out.push(loc);
        }
    }
    out
}

/// hybrid 双服务器语言 `.宿主` 出发的 references 主+伴生双查合并：astro `.astro`
/// 主+伴生去重合并（↖ mirror: astro_language_server.py@7a296833 `request_references`
/// `_is_astro_file` 分支）；svelte `.svelte` 同构——伴生 TS LS 挂
/// typescript-svelte-plugin，对 .svelte 消费者持有完整 TS program 图（↖ mirror:
/// svelte_language_server.py@7a296833 `SvelteTypeScriptServer` 类文档；上游另配的
/// `$/getComponentReferences` 增补为增量召回，未抄，见 svelte.rs 头部 Δ 记录）。
/// 伴生缺失/查询失败降级主查结果（上游 try/except 同款，不新增失败模式）；
/// 不适用场景原样返回 primary（其余语言逐字节不变）。
async fn fetch_references_with_hybrid_companion(
    session: &Arc<Session>,
    root: &Path,
    abs_file: &Path,
    line: u32,
    col: u32,
    primary: Vec<Location>,
) -> Vec<Location> {
    if !hybrid_companion_applicable(&session.language_id(), abs_file) {
        return primary;
    }
    let Some(companion) =
        ls_registry::adapter_for(&session.language_id()).and_then(|a| a.semantic_session(root))
    else {
        tracing::warn!(
            root = %root.display(),
            "hybrid companion semantic session unavailable; using primary only"
        );
        return primary;
    };
    match fetch_references(&companion, abs_file, line, col).await {
        Ok(companion_refs) => merge_reference_locations(primary, companion_refs),
        Err(e) => {
            tracing::warn!(
                file = %abs_file.display(),
                error = %e,
                "hybrid companion TS references failed; falling back to primary only"
            );
            primary
        }
    }
}

pub async fn find_referencing_symbols(
    session: &Arc<Session>,
    root: &Path,
    file: &str,
    line: u32,
    col: u32,
) -> RefResult<Vec<RefSymbolHit>> {
    // hybrid 双服务器语言（astro）的 per-file 路由：ts/js 系文件的引用语义只在伴生
    // TS LS（↖ mirror: astro_language_server.py@7a296833 `request_references` 对
    // `_is_ts_file` 路由伴生；主 astro-ls 对 .ts 文件 references 恒空 —— 真机帧录制
    // 实证）。`.astro` 留主会话查一次，再由 `fetch_references_with_hybrid_companion`
    // 补伴生 TS 侧引用并去重合并（上游 `_is_astro_file` 分支主+伴生双查语义）。
    // 伴生查找用 canon_root：COMPANION 槽的 key 是 `Supervisor::key` 的 canonical
    // 形态，CLI 原始 root 形态不等时 `r == root` 失配 → 伴生永远查不到。
    let canon_root = dunce::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let session = semantic_session_for_file(session, &canon_root, file);
    let abs_file = canon_root.join(file);
    let refs = fetch_references(&session, &abs_file, line, col).await?;
    let refs =
        fetch_references_with_hybrid_companion(&session, &canon_root, &abs_file, line, col, refs)
            .await;

    let mut by_file: HashMap<String, Vec<(u32, u32)>> = HashMap::new();
    for loc in refs {
        let abs = match uri_to_path(loc.uri.as_str()) {
            Some(p) => p,
            None => continue,
        };
        let rel = abs
            .strip_prefix(&canon_root)
            .unwrap_or(&abs)
            .to_string_lossy()
            .replace('\\', "/");
        let (line, col) = (loc.range.start.line, loc.range.start.character);
        by_file.entry(rel).or_default().push((line, col));
    }

    let mut out = Vec::new();
    let mut seen: HashSet<(String, String, u32, u32)> = HashSet::new();
    for (rel, positions) in by_file {
        let abs = canon_root.join(&rel);
        let symbols = fetch_document_symbols(&session, &abs)
            .await
            .unwrap_or_default();
        for (line, col) in positions {
            let container = find_container_name(&symbols, line, col);
            let key = (rel.clone(), container.clone(), line, col);
            if seen.insert(key) {
                out.push(RefSymbolHit {
                    file: rel.clone(),
                    line,
                    col,
                    container_name: container,
                });
            }
        }
    }
    Ok(out)
}

/// 单个分组容器：相同 (container_name, file) 的 refs 聚合。
#[derive(Debug, Serialize)]
pub struct RefGroup {
    /// 外层符号名（类/方法/函数名）。顶层时为空字符串（与 RefSymbolHit 语义一致）。
    pub container: String,
    pub file: String,
    pub count: usize,
    /// 容器下前 3 条 ref 样本（保留完整 RefSymbolHit 字段）。
    pub samples: Vec<RefSymbolHit>,
}

/// `find-referencing-symbols --grouped` 的分组翻页报告。
#[derive(Debug, Serialize)]
pub struct GroupedRefReport {
    /// 原始 ref 总数（未分页）。
    pub total: usize,
    /// 全部 group 数（未分页）。
    pub group_count: usize,
    /// 1-based 当前页号。
    pub page: usize,
    pub page_size: usize,
    pub groups: Vec<RefGroup>,
}

/// 按 (container_name, file) 分桶聚合：每桶保留前 3 条样本，按 BTreeMap 排序保证
/// 跨页顺序稳定；page/page_size 1-based 翻页（page 越界返回空 groups）。
///
/// ponytail: 单次 in-memory 分桶；hits 万级以下足够。再大需要外部 sort。
pub fn group_refs(hits: Vec<RefSymbolHit>, page: usize, page_size: usize) -> GroupedRefReport {
    use std::collections::BTreeMap;
    let total = hits.len();
    let mut buckets: BTreeMap<(String, String), Vec<RefSymbolHit>> = BTreeMap::new();
    for h in hits {
        let key = (h.container_name.clone(), h.file.clone());
        buckets.entry(key).or_default().push(h);
    }
    let all_groups: Vec<RefGroup> = buckets
        .into_iter()
        .map(|((container, file), mut hits)| {
            let count = hits.len();
            hits.truncate(3);
            RefGroup {
                container,
                file,
                count,
                samples: hits,
            }
        })
        .collect();
    let group_count = all_groups.len();
    let start = page.saturating_sub(1).saturating_mul(page_size);
    let groups: Vec<RefGroup> = all_groups.into_iter().skip(start).take(page_size).collect();
    GroupedRefReport {
        total,
        group_count,
        page,
        page_size,
        groups,
    }
}

pub async fn find_referencing_code_snippets(
    session: &Arc<Session>,
    root: &Path,
    file: &str,
    line: u32,
    col: u32,
    context_lines: u32,
    max_results: usize,
) -> RefResult<Vec<RefSnippetHit>> {
    let canon_root = dunce::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let session = semantic_session_for_file(session, &canon_root, file);
    let abs_file = canon_root.join(file);
    let refs = fetch_references(&session, &abs_file, line, col).await?;
    let refs =
        fetch_references_with_hybrid_companion(&session, &canon_root, &abs_file, line, col, refs)
            .await;

    let mut cache: HashMap<String, String> = HashMap::new();
    let mut out = Vec::new();
    let mut seen: HashSet<(String, u32, u32)> = HashSet::new();
    for loc in refs {
        if out.len() >= max_results {
            break;
        }
        let abs = match uri_to_path(loc.uri.as_str()) {
            Some(p) => p,
            None => continue,
        };
        let rel = abs
            .strip_prefix(&canon_root)
            .unwrap_or(&abs)
            .to_string_lossy()
            .replace('\\', "/");
        let (line, col) = (loc.range.start.line, loc.range.start.character);
        let key = (rel.clone(), line, col);
        if !seen.insert(key) {
            continue;
        }
        let content = match cache.get(&rel) {
            Some(c) => c.clone(),
            None => match tokio::fs::read_to_string(&abs).await {
                Ok(c) => {
                    cache.insert(rel.clone(), c.clone());
                    c
                }
                Err(_) => continue,
            },
        };
        let line_idx = line as usize;
        let lines: Vec<&str> = content.lines().collect();
        if line_idx >= lines.len() {
            continue;
        }
        let text = lines[line_idx].trim_end().to_string();
        let n = context_lines as usize;
        let lo = line_idx.saturating_sub(n);
        let hi = (line_idx + n + 1).min(lines.len());
        let snippet = lines[lo..hi].join("\n");
        out.push(RefSnippetHit {
            file: rel,
            line,
            col,
            text,
            snippet,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn hit(file: &str, container: &str, line: u32, col: u32) -> RefSymbolHit {
        RefSymbolHit {
            file: file.into(),
            line,
            col,
            container_name: container.into(),
        }
    }

    #[test]
    fn group_refs_aggregates_by_container_and_file() {
        let hits = vec![
            hit("a.rs", "Foo", 1, 0),
            hit("a.rs", "Foo", 2, 0),
            hit("b.rs", "Foo", 3, 0),
            hit("a.rs", "Bar", 4, 0),
            hit("c.rs", "", 5, 0),
        ];
        let r = group_refs(hits, 1, 20);
        assert_eq!(r.total, 5);
        // 4 buckets: (Foo,a), (Foo,b), (Bar,a), (None-as-empty,c)
        assert_eq!(r.group_count, 4);
        // BTreeMap key (String,String) 排序：空串 "" 排在所有非空前 → 第一组
        assert!(r.groups[0].container.is_empty());
        assert_eq!(r.groups[0].file, "c.rs");
        assert_eq!(r.groups[0].count, 1);
        // (Foo, a) 桶：2 条
        let foo_a = r
            .groups
            .iter()
            .find(|g| g.container == "Foo" && g.file == "a.rs")
            .expect("Foo/a bucket");
        assert_eq!(foo_a.count, 2);
    }

    #[test]
    fn group_refs_pagination_works() {
        // 50 个不同 (container,file) → 50 groups；page_size=20 → 20/20/10。
        let mk = || -> Vec<RefSymbolHit> {
            (0..50)
                .map(|i| hit(&format!("f{i}.rs"), &format!("C{i}"), i, 0))
                .collect()
        };
        let p1 = group_refs(mk(), 1, 20);
        let p2 = group_refs(mk(), 2, 20);
        let p3 = group_refs(mk(), 3, 20);
        assert_eq!(p1.group_count, 50);
        assert_eq!(p1.groups.len(), 20);
        assert_eq!(p2.groups.len(), 20);
        assert_eq!(p3.groups.len(), 10);
        // 翻页互不重叠 + 顺序稳定（BTreeMap key 已保序）
        assert_ne!(p1.groups[0].file, p2.groups[0].file);
        assert_ne!(p2.groups[0].file, p3.groups[0].file);
    }

    #[test]
    fn samples_capped_at_three_per_group() {
        let hits: Vec<RefSymbolHit> = (0..10).map(|i| hit("x.rs", "C", i, 0)).collect();
        let r = group_refs(hits, 1, 20);
        assert_eq!(r.group_count, 1);
        assert_eq!(r.groups[0].count, 10);
        assert_eq!(r.groups[0].samples.len(), 3);
        // sample 保留前 3 条
        assert_eq!(r.groups[0].samples[0].line, 0);
        assert_eq!(r.groups[0].samples[1].line, 1);
        assert_eq!(r.groups[0].samples[2].line, 2);
    }

    fn loc(uri: &str, line: u32, col: u32) -> Location {
        Location {
            uri: lsp_types::Uri::from_str(uri).expect("valid uri"),
            range: Range {
                start: Position::new(line, col),
                end: Position::new(line, col),
            },
        }
    }

    #[test]
    fn merge_reference_locations_dedupes_overlapping() {
        let primary = vec![loc("file:///w/a.astro", 3, 4), loc("file:///w/b.ts", 5, 6)];
        let companion = vec![
            loc("file:///w/b.ts", 5, 6), // 与 primary 重叠 → 舍弃伴生份
            loc("file:///w/c.ts", 7, 8),
        ];
        let merged = merge_reference_locations(primary, companion);
        let keys: Vec<(String, u32, u32)> = merged
            .iter()
            .map(|l| {
                (
                    l.uri.as_str().to_string(),
                    l.range.start.line,
                    l.range.start.character,
                )
            })
            .collect();
        assert_eq!(
            keys,
            vec![
                ("file:///w/a.astro".into(), 3, 4),
                ("file:///w/b.ts".into(), 5, 6),
                ("file:///w/c.ts".into(), 7, 8),
            ]
        );
    }

    #[test]
    fn merge_reference_locations_companion_missing_degrades_to_primary() {
        let primary = vec![loc("file:///w/a.astro", 3, 4)];
        // 伴生缺失（空）→ 主查结果原样保留，不新增也不丢条目
        let merged = merge_reference_locations(primary, vec![]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].uri.as_str(), "file:///w/a.astro");
        assert_eq!(merged[0].range.start.line, 3);
    }

    #[test]
    fn hybrid_companion_applicable_gates_host_language_and_extension() {
        assert!(hybrid_companion_applicable(
            "astro",
            Path::new("src/pages/index.astro")
        ));
        // 扩展名大小写不敏感（Windows 常见）
        assert!(hybrid_companion_applicable(
            "astro",
            Path::new("src/pages/index.ASTRO")
        ));
        // ts/js 系文件走纯伴生路由，不进双查
        assert!(!hybrid_companion_applicable(
            "astro",
            Path::new("src/utils/fmt.ts")
        ));
        // 非 astro 宿主语言不动
        assert!(!hybrid_companion_applicable(
            "typescript",
            Path::new("src/pages/index.astro")
        ));
        assert!(!hybrid_companion_applicable(
            "rust",
            Path::new("src/lib.rs")
        ));
        // W2 批：svelte 宿主语言 + .svelte 目标文件进双查；.scss/.ts 不进。
        assert!(hybrid_companion_applicable(
            "svelte",
            Path::new("src/routes/about.svelte")
        ));
        assert!(hybrid_companion_applicable(
            "svelte",
            Path::new("App.SVELTE")
        ));
        assert!(!hybrid_companion_applicable(
            "svelte",
            Path::new("style.scss")
        ));
        assert!(!hybrid_companion_applicable(
            "svelte",
            Path::new("src/utils/fmt.ts")
        ));
    }
}
