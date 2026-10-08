//! per-LS 符号归一 quirk（上游对拍采纳 W3b 批，bd 69e batchA/B/C 五门）。
//!
//! 上游在 solidlsp 符号构建层（`ls.py` `_build_document_symbols_from_raw_symbols`
//! 逐符号调 `_normalize_symbol_name`）做 per-LS 修正；我方对应出口 = supervisor 的
//! documentSymbol 平面化/寻址路径，经 [`normalize_symbol_name`] /
//! [`fix_fortls_selection_ranges`] 按 `LanguageId` 分派（纯函数，supervisor 直调，
//! 不经 adapter trait——五门中四门是 T0 无 adapter 实例）。
//!
//! 覆盖（各带上游行号锚 + 单测）：
//! - erlang：`name/arity` 的 `/` → `#`（`/` 是 Serena name-path 分隔符，保留则符号
//!   永不可寻址）↖ mirror erlang_language_server.py@7a296833 `ARITY_SEPARATOR` L23、
//!   `_normalize_symbol_name` L79（无条件，不分 kind）。
//! - lua：`M.foo`/`mod:fn` → 末段（Function/Method）↖ mirror lua_ls.py@7a296833
//!   `_normalize_symbol_name` L207-220。
//! - swift：剥函数名 `(` 后缀（Function/Method/Constructor）↖ mirror
//!   sourcekit_lsp.py@7a296833 `_normalize_symbol_name` L63-77。
//! - nextflow：声明关键字前缀剥离（复用 [`crate::nextflow::strip_symbol_prefix`]，
//!   ↖ mirror nextflow_language_server.py `_normalize_symbol_name`）。
//! - fortran：fortls selectionRange 行首 bug 修正 ↖ mirror
//!   fortran_language_server.py@7a296833 `_fix_fortls_selection_range` L48-160 +
//!   `_build_document_symbols_from_raw_symbols` 覆写（cache fingerprint v2）。

use lsp_types::{DocumentSymbol, SymbolKind};

/// documentSymbol 名字归一（per-LS quirk 分派；未覆盖语言恒等返回原样）。
///
/// `lang` = 内部语言名（`Session::language_id()` 形态，如 "erlang"）。
pub fn normalize_symbol_name(lang: &str, name: &str, kind: SymbolKind) -> String {
    match lang {
        // ↖ mirror: `symbol["name"].replace("/", ARITY_SEPARATOR)`，ARITY_SEPARATOR
        // = "#"（erlang_language_server.py@7a296833 L23/L79；无条件）。
        "erlang" => name.replace('/', "#"),
        // ↖ mirror: lua_ls.py@7a296833 L207-220（Function/Method 限定；"." 优先于 ":"）。
        "lua" => {
            if !matches!(kind, SymbolKind::FUNCTION | SymbolKind::METHOD) {
                return name.to_string();
            }
            if let Some(pos) = name.rfind('.') {
                return name[pos + 1..].to_string();
            }
            if let Some(pos) = name.rfind(':') {
                return name[pos + 1..].to_string();
            }
            name.to_string()
        }
        // ↖ mirror: sourcekit_lsp.py@7a296833 L63-77（Function/Method/Constructor
        // 限定；首个 "(" 起剥除并去尾空白）。
        "swift" => {
            if !matches!(
                kind,
                SymbolKind::FUNCTION | SymbolKind::METHOD | SymbolKind::CONSTRUCTOR
            ) {
                return name.to_string();
            }
            match name.split_once('(') {
                Some((head, _)) => head.trim_end().to_string(),
                None => name.to_string(),
            }
        }
        // ↖ mirror: nextflow `_normalize_symbol_name` / `_SYMBOL_NAME_PREFIXES`
        //（"process GREET" → "GREET"；实现与单测在 nextflow.rs，此处分派）。
        "nextflow" => crate::nextflow::strip_symbol_prefix(name).to_string(),
        _ => name.to_string(),
    }
}

/// Fortran 构造行的标识符解析结果（行内 byte 偏移 + 长度）。
fn fortran_identifier_at(line: &str) -> Option<(usize, usize)> {
    let trimmed = line.trim_start();
    let first = trimmed.chars().next()?;
    if !first.is_ascii_alphabetic() && first != '_' {
        return None;
    }
    let word_end = trimmed
        .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .unwrap_or(trimmed.len());
    let keyword = &trimmed[..word_end];

    // 识别顺序 = 上游正则尝试序：type → 标准关键字 → submodule。
    // origin 一律 = 段在原行的绝对起点（tail 后缀链：line.len() - seg.len()），
    // identifier_at_offset 内部再吃段内前导空白。
    let rest = &trimmed[word_end..];
    if keyword.eq_ignore_ascii_case("type") {
        // ↖ mirror type_pattern `^\s*type\s*(?:,.*?)?\s*(?:::)?\s*([a-zA-Z_]\w*)`：
        // 惰性 `,*?` 短匹配使上游在带 `,` 修饰时取 `,` 后第一个标识符（含
        // "extends"/"parameter" 等修饰词——上游注释声称取真名但正则实取首词，
        // mirror 保留同语义）；无 `,` 时跳过可选 `::` 取标识符。
        let after = rest.trim_start();
        if let Some(stripped) = after.strip_prefix(',') {
            return identifier_at_offset(stripped, line.len() - stripped.len());
        }
        let after2 = after.strip_prefix("::").unwrap_or(after);
        identifier_at_offset(after2, line.len() - after2.len())
    } else if matches!(
        keyword.to_ascii_lowercase().as_str(),
        "function" | "subroutine" | "module" | "program" | "interface"
    ) {
        // ↖ mirror standard_pattern：关键字 + `\s+` + 标识符（无空白 = 不匹配）。
        let after = rest.trim_start();
        if after.len() == rest.len() {
            return None; // `\s+` 要求至少一个空白
        }
        identifier_at_offset(after, line.len() - after.len())
    } else if keyword.eq_ignore_ascii_case("submodule") {
        // ↖ mirror submodule_pattern `^\s*submodule\s*\([^)]+\)\s+([a-zA-Z_]\w*)`。
        let after = rest.trim_start();
        let after = after.strip_prefix('(')?;
        let close = after.find(')')?;
        let tail = after[close + 1..].trim_start();
        identifier_at_offset(tail, line.len() - tail.len())
    } else {
        None
    }
}

/// 在 `s` 起始处解析 `[a-zA-Z_]\w*`，偏移换算基于 `origin`（s 在原行内的起点）。
fn identifier_at_offset(s: &str, origin: usize) -> Option<(usize, usize)> {
    let trimmed = s.trim_start();
    let lead = s.len() - trimmed.len();
    let end = trimmed
        .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .unwrap_or(trimmed.len());
    if end == 0 {
        return None;
    }
    let first = trimmed[..1].chars().next()?;
    if !first.is_ascii_alphabetic() && first != '_' {
        return None;
    }
    Some((origin + lead, end))
}

/// 修正 fortls 的 selectionRange 行首 bug（递归全树）。
///
/// ↖ mirror: fortran_language_server.py@7a296833 `_build_document_symbols_from_raw_symbols`
/// 覆写（`fix_symbol_and_children` 递归）+ `_fix_fortls_selection_range`（读
/// selectionRange.start.line 对应源行重解析标识符位置，命中即覆盖；不匹配原样返回）。
/// 上游同步修 `_document_symbols_cache_fingerprint` = v2——我方符号缓存键含文件 mtime，
/// 语义等价无需版本位。
pub fn fix_fortls_selection_ranges(symbols: &mut [DocumentSymbol], content: &str) {
    let lines: Vec<&str> = content.lines().collect();
    fix_all(symbols, &lines);
}

fn fix_all(symbols: &mut [DocumentSymbol], lines: &[&str]) {
    for sym in symbols {
        fix_one(sym, lines);
    }
}

fn fix_one(sym: &mut DocumentSymbol, lines: &[&str]) {
    let start_line = sym.selection_range.start.line as usize;
    if let Some(line) = lines.get(start_line)
        && let Some((off, len)) = fortran_identifier_at(line)
    {
        sym.selection_range.start.character = char_column(line, off);
        sym.selection_range.end.character = char_column(line, off) + len as u32;
    }
    if let Some(children) = sym.children.as_mut() {
        fix_all(children, lines);
    }
}

/// byte 偏移 → UTF-16 列（LSP position encoding；Fortran 源码 ASCII 为主，
/// 非 ASCII 行按 utf-16 计与 lsp-core position 编码声明一致）。
fn char_column(line: &str, byte_offset: usize) -> u32 {
    line[..byte_offset]
        .chars()
        .map(|c| c.len_utf16() as u32)
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn erlang_replaces_arity_slash() {
        // ↖ mirror erlang_language_server.py@7a296833 L79（无条件，不分 kind）。
        assert_eq!(
            normalize_symbol_name("erlang", "create_user/2", SymbolKind::FUNCTION),
            "create_user#2"
        );
        assert_eq!(
            normalize_symbol_name("erlang", "t()/2", SymbolKind::TYPE_PARAMETER),
            "t()#2"
        );
        assert_eq!(
            normalize_symbol_name("erlang", "plain", SymbolKind::MODULE),
            "plain"
        );
    }

    #[test]
    fn lua_strips_module_prefix_for_functions_only() {
        let f = SymbolKind::FUNCTION;
        let m = SymbolKind::METHOD;
        let v = SymbolKind::VARIABLE;
        // ↖ mirror lua_ls.py@7a296833 L207-220。
        assert_eq!(normalize_symbol_name("lua", "M.foo", f), "foo");
        assert_eq!(normalize_symbol_name("lua", "a.b.c", m), "c");
        assert_eq!(normalize_symbol_name("lua", "mod:fn", f), "fn");
        // 末段语义："." 优先（↖ mirror if "." in original 分支在前——"x.y:z" 实返
        // "y:z"，上游同）。
        assert_eq!(normalize_symbol_name("lua", "x.y:z", f), "y:z");
        // 非 Function/Method 原样。
        assert_eq!(normalize_symbol_name("lua", "M.foo", v), "M.foo");
        // 无分隔符原样。
        assert_eq!(normalize_symbol_name("lua", "plain", f), "plain");
    }

    #[test]
    fn swift_strips_paren_suffix_for_callables_only() {
        let f = SymbolKind::FUNCTION;
        let c = SymbolKind::CONSTRUCTOR;
        let v = SymbolKind::VARIABLE;
        // ↖ mirror sourcekit_lsp.py@7a296833 L63-77。
        assert_eq!(normalize_symbol_name("swift", "handle(req)", f), "handle");
        assert_eq!(normalize_symbol_name("swift", "init()", c), "init");
        // 无 "(" 原样。
        assert_eq!(normalize_symbol_name("swift", "plain", f), "plain");
        // 非 callable kind 原样。
        assert_eq!(
            normalize_symbol_name("swift", "handle(req)", v),
            "handle(req)"
        );
    }

    #[test]
    fn nextflow_dispatches_to_prefix_strip() {
        assert_eq!(
            normalize_symbol_name("nextflow", "process GREET", SymbolKind::FUNCTION),
            "GREET"
        );
    }

    #[test]
    fn unknown_lang_is_identity() {
        assert_eq!(
            normalize_symbol_name("rust", "M.foo", SymbolKind::FUNCTION),
            "M.foo"
        );
    }

    fn sym_with_sel(line: u32, s: u32, e: u32) -> DocumentSymbol {
        DocumentSymbol {
            name: "x".into(),
            detail: None,
            kind: SymbolKind::FUNCTION,
            tags: None,
            #[allow(deprecated)]
            deprecated: None,
            range: lsp_types::Range::new(
                lsp_types::Position::new(line, 0),
                lsp_types::Position::new(line + 1, 0),
            ),
            selection_range: lsp_types::Range::new(
                lsp_types::Position::new(line, s),
                lsp_types::Position::new(line, e),
            ),
            children: None,
        }
    }

    #[test]
    fn fortran_fixes_line_start_selection_for_keywords() {
        // ↖ mirror _fix_fortls_selection_range docstring 样例。
        let content =
            "module math_utils\nfunction add_numbers(a, b) result(sum)\nend function\nend module";
        let mut syms = vec![
            sym_with_sel(0, 0, 5), // module 行首锚 → 修到 7..17
            sym_with_sel(1, 0, 3), // function 行首锚 → 修到 9..20
        ];
        fix_fortls_selection_ranges(&mut syms, content);
        assert_eq!(syms[0].selection_range.start.character, 7);
        assert_eq!(syms[0].selection_range.end.character, 17);
        assert_eq!(syms[1].selection_range.start.character, 9);
        assert_eq!(syms[1].selection_range.end.character, 20);
    }

    #[test]
    fn fortran_handles_type_and_submodule_forms() {
        let content =
            "type point\n  real :: x\nend type\ntype :: color\nsubmodule (parent_mod) child_mod";
        let mut syms = vec![
            sym_with_sel(0, 0, 4), // "type point" → point @5..10
            sym_with_sel(3, 0, 4), // "type :: color" → color @8..13
            sym_with_sel(4, 0, 4), // submodule (parent_mod) child_mod → child_mod
        ];
        fix_fortls_selection_ranges(&mut syms, content);
        assert_eq!(syms[0].selection_range.start.character, 5);
        assert_eq!(syms[0].selection_range.end.character, 10);
        assert_eq!(syms[1].selection_range.start.character, 8);
        assert_eq!(syms[2].selection_range.start.character, 23);
        assert_eq!(syms[2].selection_range.end.character, 32);
    }

    #[test]
    fn fortran_leaves_unmatched_lines_alone() {
        // 变量行 / 非构造行不修正（上游 "variables, which don't have this pattern"）。
        let content = "integer :: x\nx = do_something(y)";
        let mut syms = vec![sym_with_sel(0, 3, 4), sym_with_sel(1, 0, 1)];
        fix_fortls_selection_ranges(&mut syms, content);
        assert_eq!(syms[0].selection_range.start.character, 3);
        assert_eq!(syms[1].selection_range.start.character, 0);
    }

    #[test]
    fn fortran_recurses_into_children() {
        // ↖ mirror fix_symbol_and_children 递归。
        let content = "module m\ncontains\n  function f()\n  end function\nend module";
        let inner = sym_with_sel(2, 2, 5);
        let mut outer = sym_with_sel(0, 0, 3);
        outer.children = Some(vec![inner]);
        let mut syms = vec![outer];
        fix_fortls_selection_ranges(&mut syms, content);
        let kids = syms[0].children.as_ref().unwrap();
        assert_eq!(syms[0].selection_range.start.character, 7);
        assert_eq!(kids[0].selection_range.start.character, 11);
    }
}
