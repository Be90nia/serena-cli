//! bd serena-rust-vqdg：写类工具符号名回退链。
//!
//! 盲测 P1-1：find-symbol 的输出名 / 用户直觉写法喂给写类工具（symbol-body /
//! replace-body / replace-text-in-symbol / edit-context）时 BAD_ARGS——
//! documentSymbol 的 LS 名与 workspace/symbol 输出名形态脱节（Go `Triangle.Area`
//! vs `(*Triangle).Area`、PowerShell `Format-LogLine` vs `function Format-LogLine ()`、
//! bash 裸名 vs `greet()`）。
//!
//! 铁律：**精确名行为零变化**——仅 normalize 后精确匹配失败才启用回退；回退
//! 唯一命中才生效；多命中报歧义列候选；命中解释随响应 warning 透出（禁静默猜）。

use ls_adapters::symbol_quirks;
use lsp_types::{DocumentSymbolResponse, Range};

/// 单个候选符号（解析输入）。
pub(crate) struct Entry {
    /// 匹配键名（lib.rs 侧 = normalize 后 LS 名；edit_tools 侧 = 裸 LS 名，
    /// 与其精确层现状一致）。
    pub name: String,
    pub range: Range,
}

/// 灵活解析结果。
#[derive(Debug)]
pub(crate) enum Resolved {
    /// 精确命中（现状路径，无回退说明）。
    Exact { range: Range },
    /// 回退命中——`note` 说明命中了哪种解释（调用方透出响应 warning）；
    /// `ls_name` = LS 侧真名（body 内定位用，Go 回退场景与用户输入名不同）。
    Fallback {
        range: Range,
        ls_name: String,
        note: String,
    },
    /// 回退多命中——候选描述（LS 名 + 规则），调用方组装歧义错误。
    Ambiguous { candidates: Vec<String> },
    /// 精确与回退都未命中。
    NotFound,
}

/// 候选名生成（输入 = normalize 后的 LS 名）。返回 (候选名, 规则描述)，
/// 不含原名本身（精确层已查）。
fn fallback_candidates(lang: &str, name: &str) -> Vec<(String, &'static str)> {
    let mut out: Vec<(String, &'static str)> = Vec::new();
    let push = |out: &mut Vec<(String, &'static str)>, cand: String, why: &'static str| {
        if !cand.is_empty() && cand != name && !out.iter().any(|(e, _)| *e == cand) {
            out.push((cand, why));
        }
    };
    // 通用：尾部调用括号剥除（bash `greet()`、PS `function F ()`、`F ()`）。
    // 剥后含 `(` 说明剥错段（C++ `operator()` 类的防御），放弃。
    let trimmed = name.trim_end();
    if let Some(head) = trimmed.strip_suffix("()") {
        let head = head.trim_end();
        if !head.contains('(') {
            push(&mut out, head.to_string(), "call-suffix () stripped");
            if matches!(lang, "powershell" | "bash")
                && let Some(bare) = head.strip_prefix("function ")
            {
                push(
                    &mut out,
                    bare.trim_start().to_string(),
                    "function prefix + () stripped",
                );
            }
        }
    }
    // PowerShell / bash：独立 `function foo` 前缀形态。
    if matches!(lang, "powershell" | "bash")
        && let Some(bare) = trimmed.strip_prefix("function ")
    {
        let bare = bare.trim_start();
        if !bare.is_empty() {
            push(&mut out, bare.to_string(), "function prefix stripped");
        }
    }
    // Go：receiver 剥除 `(*Triangle).Area` / `(Triangle).Area` → `Triangle.Area`
    // （gopls workspace/symbol 输出形态，find-symbol 消费者的直觉写法）。
    if lang == "go"
        && let Some(rest) = name.strip_prefix('(')
        && let Some(sep) = rest.find(").")
    {
        let recv = rest[..sep].trim().trim_start_matches('*').trim();
        let method = rest[sep + 2..].trim();
        if !recv.is_empty() && !method.is_empty() && !recv.contains('(') {
            push(
                &mut out,
                format!("{recv}.{method}"),
                "go receiver prefix stripped",
            );
        }
    }
    out
}

/// documentSymbol 树（Nested/Flat）→ 回退链候选列表。
///
/// `normalize=true`（lib.rs documentSymbol 路径）：名字过 per-LS quirk 归一，
/// 与 `find_symbol_range` 精确键同源；`normalize=false`（edit_tools 路径）：裸
/// LS 名，与其精确层（`walk_nested` 裸等值）同源——两条路径各自的精确行为
/// 零变化，回退层与各自精确层同键生成候选。
pub(crate) fn collect_entries(
    resp: Option<&DocumentSymbolResponse>,
    lang: &str,
    normalize: bool,
) -> Vec<Entry> {
    use lsp_types::DocumentSymbol;
    fn walk(items: &[DocumentSymbol], lang: &str, normalize: bool, out: &mut Vec<Entry>) {
        for it in items {
            let name = if normalize {
                symbol_quirks::normalize_symbol_name(lang, &it.name, it.kind)
            } else {
                it.name.clone()
            };
            out.push(Entry {
                name,
                range: it.range,
            });
            if let Some(children) = it.children.as_ref() {
                walk(children, lang, normalize, out);
            }
        }
    }
    let mut out = Vec::new();
    match resp {
        Some(DocumentSymbolResponse::Nested(items)) => walk(items, lang, normalize, &mut out),
        Some(DocumentSymbolResponse::Flat(items)) => {
            for it in items {
                let name = if normalize {
                    symbol_quirks::normalize_symbol_name(lang, &it.name, it.kind)
                } else {
                    it.name.clone()
                };
                out.push(Entry {
                    name,
                    range: it.location.range,
                });
            }
        }
        None => {}
    }
    out
}

/// 灵活解析：精确层（首个等值命中即返回，序与既有 walk 一致）→ 回退层
/// （唯一命中才生效）。
pub(crate) fn resolve(entries: &[Entry], symbol: &str, lang: &str) -> Resolved {
    for e in entries {
        if e.name == symbol {
            return Resolved::Exact { range: e.range };
        }
    }
    // 双向回退：A) LS 名衍生候选 == 用户输入（树内 `(*Triangle).Area` vs 输入
    // `Triangle.Area` / `function F ()` vs 输入 `F`）；B) 用户输入衍生候选 == LS
    // 名（输入 `greet_user()` vs 树内裸名 `greet_user`）。两侧同一规则表，唯一
    // 命中才生效。
    let input_cands = fallback_candidates(lang, symbol);
    let mut hits: Vec<(Range, String, &'static str)> = Vec::new();
    for e in entries {
        let mut why_hit: Option<&'static str> = None;
        for (cand, why) in fallback_candidates(lang, &e.name) {
            if cand == symbol {
                why_hit = Some(why);
                break;
            }
        }
        if why_hit.is_none() {
            for (cand, why) in &input_cands {
                if *cand == e.name {
                    why_hit = Some(why);
                    break;
                }
            }
        }
        if let Some(why) = why_hit {
            hits.push((e.range, e.name.clone(), why));
        }
    }
    match hits.len() {
        0 => Resolved::NotFound,
        1 => {
            let (range, ls_name, why) = hits.remove(0);
            Resolved::Fallback {
                range,
                note: format!(
                    "symbol `{symbol}` not matched exactly; resolved via fallback to `{ls_name}` ({why})"
                ),
                ls_name,
            }
        }
        _ => Resolved::Ambiguous {
            candidates: hits
                .into_iter()
                .map(|(_, name, why)| format!("{name} ({why})"))
                .collect(),
        },
    }
}

/// symbol-body 类工具的返回载体：body 文本 + 回退命中的附加信息
///（精确命中时 ls_name/note 均 None——wire 零变化）。
pub struct SymbolBodyHit {
    pub body: String,
    pub ls_name: Option<String>,
    pub note: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(l: u32) -> Range {
        Range {
            start: lsp_types::Position {
                line: l,
                character: 0,
            },
            end: lsp_types::Position {
                line: l,
                character: 1,
            },
        }
    }

    fn entry(name: &str, l: u32) -> Entry {
        Entry {
            name: name.to_string(),
            range: range(l),
        }
    }

    // ---- 候选生成：Go 双形态 ----

    #[test]
    fn go_pointer_receiver_strips_to_dotted() {
        let c = fallback_candidates("go", "(*Triangle).Area");
        assert_eq!(
            c,
            vec![("Triangle.Area".to_string(), "go receiver prefix stripped")]
        );
    }

    #[test]
    fn go_value_receiver_strips_to_dotted() {
        let c = fallback_candidates("go", "(Triangle).Area");
        assert_eq!(c[0].0, "Triangle.Area");
    }

    #[test]
    fn go_plain_name_has_no_candidates() {
        assert!(fallback_candidates("go", "Triangle.Area").is_empty());
    }

    // ---- 候选生成：PowerShell / Bash ----

    #[test]
    fn powershell_signature_strips_to_bare_name() {
        let c = fallback_candidates("powershell", "function Format-LogLine ()");
        assert!(c.iter().any(|(n, _)| n == "Format-LogLine"));
    }

    #[test]
    fn powershell_bare_call_suffix_strips() {
        let c = fallback_candidates("powershell", "Format-LogLine ()");
        assert!(c.iter().any(|(n, _)| n == "Format-LogLine"));
    }

    #[test]
    fn bash_call_suffix_strips() {
        let c = fallback_candidates("bash", "greet_user()");
        assert_eq!(
            c,
            vec![("greet_user".to_string(), "call-suffix () stripped")]
        );
    }

    #[test]
    fn bash_function_prefix_strips() {
        let c = fallback_candidates("bash", "function greet_user");
        assert!(c.iter().any(|(n, _)| n == "greet_user"));
    }

    // ---- 防误伤 ----

    #[test]
    fn tail_strip_skips_when_head_contains_paren() {
        // `operator()` 剥尾得 `operator` 合理；但 `foo(x)()` 剥尾含 `(` → 放弃。
        assert!(fallback_candidates("bash", "foo(x)()").is_empty());
    }

    #[test]
    fn other_langs_get_no_per_lang_candidates() {
        // rust：无尾部括号形态、无前缀形态 → 无候选（恒等，现状不变）。
        assert!(fallback_candidates("rust", "parse_config").is_empty());
    }

    // ---- resolve：精确层零变化 ----

    #[test]
    fn exact_hit_wins_and_closes_fallback() {
        let es = vec![
            entry("(*Triangle).Area", 1),
            entry("Triangle.Area", 5), // 若走到回退层会歧义；精确层必须直接命中
        ];
        match resolve(&es, "Triangle.Area", "go") {
            Resolved::Exact { range, .. } => assert_eq!(range.start.line, 5),
            other => panic!("expected exact, got {other:?}"),
        }
    }

    #[test]
    fn fallback_unique_hit_resolves_with_note() {
        let es = vec![entry("(*Triangle).Area", 3)];
        match resolve(&es, "Triangle.Area", "go") {
            Resolved::Fallback { range, note, .. } => {
                assert_eq!(range.start.line, 3);
                assert!(note.contains("(*Triangle).Area"), "{note}");
                assert!(note.contains("Triangle.Area"), "{note}");
            }
            other => panic!("expected fallback, got {other:?}"),
        }
    }

    #[test]
    fn fallback_multi_hit_is_ambiguous_with_candidates() {
        // 同文件两个方法都剥出 `Area` —— 用户裸名 `Area` 必须歧义拒猜。
        let es = vec![entry("(*A).Area", 1), entry("(*B).Area", 9)];
        // 注意：`(*A).Area` 的候选是 `A.Area`，裸 `Area` 不在其列 —— 构造真歧义
        // 需要两个符号剥出同名候选：两个 `Area ()`。
        let ps = vec![entry("function Area ()", 1), entry("Area ()", 9)];
        match resolve(&ps, "Area", "powershell") {
            Resolved::Ambiguous { candidates } => {
                assert_eq!(candidates.len(), 2);
                assert!(candidates[0].contains("function Area ()"));
            }
            other => panic!("expected ambiguous, got {other:?}"),
        }
        // Go 裸方法名不回退（保守：不生成纯方法名候选）。
        assert!(matches!(resolve(&es, "Area", "go"), Resolved::NotFound));
    }

    #[test]
    fn not_found_when_no_layer_hits() {
        let es = vec![entry("greet()", 2)];
        assert!(matches!(
            resolve(&es, " farewell", "bash"),
            Resolved::NotFound
        ));
    }

    #[test]
    fn direction_b_input_call_suffix_matches_bare_ls_name() {
        // 本机 bash-language-server 形态：树内裸名 `greet_user`，用户按调用形态
        // 输入 `greet_user()` → 输入侧剥 `()` 后唯一命中。
        let es = vec![entry("greet_user", 4), entry("add_numbers", 10)];
        match resolve(&es, "greet_user()", "bash") {
            Resolved::Fallback { range, note, .. } => {
                assert_eq!(range.start.line, 4);
                assert!(note.contains("greet_user"), "{note}");
            }
            other => panic!("expected fallback, got {other:?}"),
        }
    }

    #[test]
    fn direction_b_input_function_prefix_matches_bare_ls_name() {
        let es = vec![entry("Format-LogLine", 1)];
        match resolve(&es, "function Format-LogLine ()", "powershell") {
            Resolved::Fallback { range, .. } => assert_eq!(range.start.line, 1),
            other => panic!("expected fallback, got {other:?}"),
        }
    }

    #[test]
    fn exact_first_occurrence_order_preserved() {
        // 树序首个精确命中（与 find_symbol_range walk 序一致）。
        let es = vec![entry("dup", 7), entry("dup", 2)];
        match resolve(&es, "dup", "rust") {
            Resolved::Exact { range, .. } => assert_eq!(range.start.line, 7),
            other => panic!("expected exact, got {other:?}"),
        }
    }
}
