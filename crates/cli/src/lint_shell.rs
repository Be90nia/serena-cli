//! `lint-shell` —— 静态自审即将在 shell 执行的命令串（bd serena-rust-8ot）。
//!
//! 检测面（设计拍板 2026-09-23）：
//! ① cli 工具名拼写（对 `enum Cmd` 全子命令清单）
//! ② position 型子命令 line/col 传 0 → error（bd serena-rust-7xv 后全 1-based）、缺失 → warning
//! ③ 命令串引用的文件路径存在性（相对 cwd 语义，找不到 → warning）
//! ④ heredoc / `python -c` 内嵌 python 轻量静态检查（subprocess 解包 → error、
//!    裸 `except:` / `os.system(` → warning）
//! ⑤ `bash -n` 语法检查（子进程；bash 不可用时降级为 info finding 并注明）。
//!
//! warn-only：默认恒 exit 0；`--strict` 下存在 error 级 finding → exit 2。

use std::path::Path;

use serde_json::json;

// ============== finding ==============

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum Severity {
    Error,
    Warning,
    Info,
}

impl Severity {
    fn as_str(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Info => "info",
        }
    }
}

#[derive(Debug)]
pub(crate) struct Finding {
    pub severity: Severity,
    pub code: &'static str,
    pub message: String,
    /// 命令串内 1-based 行号。
    pub line: usize,
}

fn err(code: &'static str, message: String, line: usize) -> Finding {
    Finding {
        severity: Severity::Error,
        code,
        message,
        line,
    }
}

fn warn(code: &'static str, message: String, line: usize) -> Finding {
    Finding {
        severity: Severity::Warning,
        code,
        message,
        line,
    }
}

fn info(code: &'static str, message: String, line: usize) -> Finding {
    Finding {
        severity: Severity::Info,
        code,
        message,
        line,
    }
}

// ============== 清单（手工同步自 main.rs） ==============

/// 手工同步自 main.rs `enum Cmd`（crates/cli/src/main.rs:92 起）。enum 加/删变体
/// 时必须同步本表——`tool_names_match_clap_enum` 单测按 clap 派生名双向锁定，
/// drift 即测试红。
const TOOL_NAMES: &[&str] = &[
    "overview",
    "symbol-tree",
    "def",
    "refs",
    "hover",
    "diagnostics",
    "find-symbol",
    "find-implementations",
    "rename-symbol",
    "search",
    "read-file",
    "list-dir",
    "find-file",
    "find-referencing-symbols",
    "find-referencing-code-snippets",
    "symbol-body",
    "edit-context",
    "repo-map",
    "warm",
    "replace-body",
    "replace-text-in-symbol",
    "insert-text-before-symbol",
    "insert-text-after-symbol",
    "delete-text-in-symbol",
    "safe-delete-symbol",
    "insert-at-line",
    "replace-lines",
    "delete-lines",
    "completion",
    "containing-symbol",
    "defining-symbol",
    "signature-help",
    "code-action",
    "format",
    "format-range",
    "inlay-hint",
    "document-highlight",
    "folding-range",
    "semantic-tokens",
    "code-lens",
    "document-link",
    "call-hierarchy",
    "type-hierarchy",
    "moniker",
    "workspace-diagnostic",
    // IDE undo/redo（事务版快照栈）+ 新建文件。
    "create-text-file",
    "undo",
    "redo",
    "status",
    "stop-all",
    "install",
    "shell",
    "doctor",
    "lint-shell",
    "wait-ready",
];

/// position 型子命令的行号形状。手工同步自 main.rs `normalize_positions`
/// （约 main.rs:1918，bd serena-rust-7xv 的权威登记处）与 `enum Cmd` 各变体
/// 的位置参数：`arity` = 期望位置参数个数，`line_args` = 位置参数序列中充当
/// 1-based 行号的下标（0 = file）。
struct ToolShape {
    tool: &'static str,
    arity: usize,
    line_args: &'static [usize],
}

const POSITION_TOOLS: &[ToolShape] = &[
    // file line col —— 与 normalize_positions 的 14 个 line+col 变体一一对应。
    ToolShape {
        tool: "def",
        arity: 3,
        line_args: &[1, 2],
    },
    ToolShape {
        tool: "refs",
        arity: 3,
        line_args: &[1, 2],
    },
    ToolShape {
        tool: "hover",
        arity: 3,
        line_args: &[1, 2],
    },
    ToolShape {
        tool: "find-implementations",
        arity: 3,
        line_args: &[1, 2],
    },
    ToolShape {
        tool: "rename-symbol",
        arity: 3,
        line_args: &[1, 2],
    },
    ToolShape {
        tool: "find-referencing-symbols",
        arity: 3,
        line_args: &[1, 2],
    },
    ToolShape {
        tool: "find-referencing-code-snippets",
        arity: 3,
        line_args: &[1, 2],
    },
    ToolShape {
        tool: "completion",
        arity: 3,
        line_args: &[1, 2],
    },
    ToolShape {
        tool: "containing-symbol",
        arity: 3,
        line_args: &[1, 2],
    },
    ToolShape {
        tool: "defining-symbol",
        arity: 3,
        line_args: &[1, 2],
    },
    ToolShape {
        tool: "signature-help",
        arity: 3,
        line_args: &[1, 2],
    },
    ToolShape {
        tool: "code-action",
        arity: 3,
        line_args: &[1, 2],
    },
    ToolShape {
        tool: "document-highlight",
        arity: 3,
        line_args: &[1, 2],
    },
    ToolShape {
        tool: "moniker",
        arity: 3,
        line_args: &[1, 2],
    },
    // file sl sc el ec
    ToolShape {
        tool: "format-range",
        arity: 5,
        line_args: &[1, 2, 3, 4],
    },
    // file start_line end_line（无 col）
    ToolShape {
        tool: "inlay-hint",
        arity: 3,
        line_args: &[1, 2],
    },
    // 行级编辑（1-based，运行时同样 BAD_ARGS）。
    ToolShape {
        tool: "insert-at-line",
        arity: 3,
        line_args: &[1],
    },
    ToolShape {
        tool: "replace-lines",
        arity: 4,
        line_args: &[1, 2],
    },
    ToolShape {
        tool: "delete-lines",
        arity: 4,
        line_args: &[1, 2],
    },
    ToolShape {
        tool: "delete-text-in-symbol",
        arity: 4,
        line_args: &[2, 3],
    },
];

/// 第一位置参数为文件/目录路径的工具（路径存在性检查面）。排除 query/pattern/
/// glob/op 型（find-symbol / search / repo-map / warm / find-file / call-hierarchy /
/// type-hierarchy，首参不是路径）与管理命令。
const PATH_FIRST_TOOLS: &[&str] = &[
    "overview",
    "symbol-tree",
    "def",
    "refs",
    "hover",
    "diagnostics",
    "find-implementations",
    "rename-symbol",
    "read-file",
    "list-dir",
    "find-referencing-symbols",
    "find-referencing-code-snippets",
    "symbol-body",
    "edit-context",
    "replace-body",
    "replace-text-in-symbol",
    "insert-text-before-symbol",
    "insert-text-after-symbol",
    "delete-text-in-symbol",
    "safe-delete-symbol",
    "insert-at-line",
    "replace-lines",
    "delete-lines",
    // 新建文件：首位置参数同样是路径。undo/redo 无位置参数，不入此表。
    "create-text-file",
    "completion",
    "containing-symbol",
    "defining-symbol",
    "signature-help",
    "code-action",
    "format",
    "format-range",
    "inlay-hint",
    "document-highlight",
    "folding-range",
    "semantic-tokens",
    "code-lens",
    "document-link",
    "moniker",
];

/// 带值的全局 flag（找 tool token 时需连值跳过）。
const GLOBAL_VALUE_FLAGS: &[&str] = &[
    "--project",
    "--lang",
    "--request-timeout",
    "--index-timeout",
    "--max-tokens",
    "--invocation-id",
];

/// flag 形式的 1-based 行号参数（read-file 等）。
const FLAG_LINE_ARGS: &[&str] = &["--start-line", "--end-line", "--line"];

// ============== tokenizer ==============

#[derive(Debug)]
struct Token {
    text: String,
    /// token 首字符在命令串中的物理行（1-based）。
    line: usize,
}

/// 简易 shell tokenizer：单双引号（引号内换行保留、双引号支持 `\"` `\\` `\$`
/// `` \` `` 转义）、`\`+换行 续行、`#` 注释、`;` `|` `&` `(` `)` `` ` `` 切分为
/// 独立 token。// ponytail: 简单正则/手写分词, tree-sitter-bash 若覆盖不足再上
fn tokenize(text: &str) -> Vec<Token> {
    let mut toks = Vec::new();
    let mut cur = String::new();
    let mut cur_line = 1usize;
    let mut tok_line = 1usize;
    // 词首（前一个字符是空白/操作符/行首）→ `#` 才是注释。
    let mut at_token_start = true;
    let mut has_tok = false;
    let mut chars = text.chars().peekable();

    fn flush(toks: &mut Vec<Token>, cur: &mut String, tok_line: usize, has_tok: &mut bool) {
        if *has_tok {
            toks.push(Token {
                text: std::mem::take(cur),
                line: tok_line,
            });
            *has_tok = false;
        }
    }

    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'\n') => {
                chars.next();
                cur_line += 1;
            }
            '\\' if has_tok => {
                if let Some(&n) = chars.peek() {
                    cur.push(n);
                    chars.next();
                }
            }
            '\n' => {
                flush(&mut toks, &mut cur, tok_line, &mut has_tok);
                cur_line += 1;
                at_token_start = true;
            }
            ' ' | '\t' | '\r' => {
                flush(&mut toks, &mut cur, tok_line, &mut has_tok);
                at_token_start = true;
            }
            '\'' => {
                if !has_tok {
                    tok_line = cur_line;
                    has_tok = true;
                }
                for c2 in chars.by_ref() {
                    if c2 == '\'' {
                        break;
                    }
                    if c2 == '\n' {
                        cur_line += 1;
                    }
                    cur.push(c2);
                }
                at_token_start = false;
            }
            '"' => {
                if !has_tok {
                    tok_line = cur_line;
                    has_tok = true;
                }
                while let Some(c2) = chars.next() {
                    match c2 {
                        '"' => break,
                        '\\' => match chars.peek() {
                            Some(n @ ('"' | '\\' | '$' | '`')) => {
                                cur.push(*n);
                                chars.next();
                            }
                            _ => cur.push('\\'),
                        },
                        '\n' => {
                            cur_line += 1;
                            cur.push(c2);
                        }
                        _ => cur.push(c2),
                    }
                }
                at_token_start = false;
            }
            ';' | '|' | '&' | '(' | ')' | '`' => {
                flush(&mut toks, &mut cur, tok_line, &mut has_tok);
                toks.push(Token {
                    text: c.to_string(),
                    line: cur_line,
                });
                at_token_start = true;
            }
            '#' if at_token_start => {
                for c2 in chars.by_ref() {
                    if c2 == '\n' {
                        cur_line += 1;
                        break;
                    }
                }
            }
            _ => {
                if !has_tok {
                    tok_line = cur_line;
                    has_tok = true;
                }
                cur.push(c);
                at_token_start = false;
            }
        }
    }
    flush(&mut toks, &mut cur, tok_line, &mut has_tok);
    toks
}

/// 命令位置：行首（新语句）或 shell 分隔符 / 关键字之后。
fn is_command_pos(toks: &[Token], i: usize) -> bool {
    if i == 0 {
        return true;
    }
    let prev = &toks[i - 1];
    prev.line != toks[i].line
        || matches!(
            prev.text.as_str(),
            ";" | "|" | "&" | "(" | ")" | "`" | "then" | "do" | "else" | "!" | "{"
        )
}

fn is_cli_exe(t: &str) -> bool {
    matches!(t, "serena-cli" | "serena-cli.exe" | "cli" | "cli.exe")
}

/// 从 exe token 之后找第一个非 flag token（带值全局 flag 连值跳过）。
fn find_tool(toks: &[Token], exe_idx: usize) -> Option<usize> {
    let mut j = exe_idx + 1;
    while j < toks.len() {
        let t = toks[j].text.as_str();
        if t.starts_with('-') && t.len() > 1 {
            j += if GLOBAL_VALUE_FLAGS.contains(&t) {
                2
            } else {
                1
            };
            continue;
        }
        return Some(j);
    }
    None
}

/// tool 之后的位置参数（停在第一个 flag）。
fn positional_args(toks: &[Token], tool_idx: usize) -> Vec<&Token> {
    let mut out = Vec::new();
    for t in &toks[tool_idx + 1..] {
        // ponytail: 遇 flag 即停（clap 混排写法会漏检为 LINE_MISSING warning，可容忍）
        if t.text.starts_with('-') && t.text.len() > 1 {
            break;
        }
        out.push(t);
    }
    out
}

// ============== 检测器 ==============

fn check_shape(tool: &Token, args: &[&Token], findings: &mut Vec<Finding>) {
    let Some(shape) = POSITION_TOOLS.iter().find(|s| s.tool == tool.text) else {
        return;
    };
    for &idx in shape.line_args {
        match args.get(idx) {
            Some(t) => {
                if t.text == "0" {
                    findings.push(err(
                        "LINE_NOT_1BASED",
                        format!(
                            "tool '{}' line/col is 1-based (got 0 at position {}); \
                             bd serena-rust-7xv 后全 1-based，传 0 运行时 BAD_ARGS exit 2",
                            tool.text,
                            idx + 1
                        ),
                        tool.line,
                    ));
                }
            }
            None => {
                findings.push(warn(
                    "LINE_MISSING",
                    format!(
                        "tool '{}' expects {} positional args (file, line, col, ...), got {}",
                        tool.text,
                        shape.arity,
                        args.len()
                    ),
                    tool.line,
                ));
                break;
            }
        }
    }
}

fn check_path_first(tool: &Token, args: &[&Token], findings: &mut Vec<Finding>) {
    if !PATH_FIRST_TOOLS.contains(&tool.text.as_str()) {
        return;
    }
    let Some(p) = args.first() else { return };
    let path = p.text.as_str();
    if path.contains('$') || path.contains('*') || path.contains('?') || path.contains('~') {
        return; // 变量/glob 展开结果无法静态判定
    }
    if !Path::new(path).exists() {
        findings.push(warn(
            "PATH_MISSING",
            format!(
                "path '{}' (tool '{}', first arg) not found relative to cwd",
                path, tool.text
            ),
            p.line,
        ));
    }
}

fn check_cli_calls(toks: &[Token], findings: &mut Vec<Finding>) {
    let mut i = 0;
    while i < toks.len() {
        if is_cli_exe(&toks[i].text)
            && is_command_pos(toks, i)
            && let Some(ti) = find_tool(toks, i)
        {
            let tool = &toks[ti];
            if !TOOL_NAMES.contains(&tool.text.as_str()) {
                findings.push(err(
                    "UNKNOWN_TOOL",
                    format!(
                        "unknown tool '{}' for '{}'; run 'serena-cli --help' to list tools",
                        tool.text, toks[i].text
                    ),
                    tool.line,
                ));
            }
            let args = positional_args(toks, ti);
            check_shape(tool, &args, findings);
            check_path_first(tool, &args, findings);
            i = ti + 1 + args.len();
            continue;
        }
        i += 1;
    }
}

/// flag 形式行号：`--start-line 0` / `--start-line=0` → error。
fn check_flag_lines(toks: &[Token], findings: &mut Vec<Finding>) {
    for (i, t) in toks.iter().enumerate() {
        let (name, val): (&str, Option<&str>) = if let Some(eq) = t.text.find('=') {
            let n = &t.text[..eq];
            (n, t.text.get(eq + 1..))
        } else {
            (t.text.as_str(), toks.get(i + 1).map(|n| n.text.as_str()))
        };
        if FLAG_LINE_ARGS.contains(&name) && val == Some("0") {
            findings.push(err(
                "LINE_NOT_1BASED",
                format!("line is 1-based: flag '{name}' got 0"),
                t.line,
            ));
        }
    }
}

// ---- 内嵌 python ----

fn is_python_cmd(line: &str) -> bool {
    let t = line.trim_start();
    let rest = t
        .strip_prefix("python3")
        .or_else(|| t.strip_prefix("python"));
    match rest {
        Some(r) => r.is_empty() || r.starts_with(|c: char| c.is_whitespace() || c == '-'),
        None => false,
    }
}

/// heredoc 定界符：`<<` + 可选 `-` + 可选引号 + `[A-Za-z_][\w]*`。
/// 字母开头排除 `a << 2` 这类位移误判。
fn heredoc_delim(line: &str) -> Option<&str> {
    let b = line.as_bytes();
    let mut i = 0;
    while i + 1 < b.len() {
        if b[i] == b'<' && b[i + 1] == b'<' {
            let mut j = i + 2;
            if j < b.len() && b[j] == b'-' {
                j += 1;
            }
            if j < b.len() && (b[j] == b'\'' || b[j] == b'"') {
                j += 1;
            }
            let s = j;
            if j < b.len() && (b[j].is_ascii_alphabetic() || b[j] == b'_') {
                while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
                    j += 1;
                }
                // 所有索引都落在 ASCII 字符边界，切片安全。
                return Some(&line[s..j]);
            }
        }
        i += 1;
    }
    None
}

fn is_ident(s: &str) -> bool {
    let mut ch = s.chars();
    matches!(ch.next(), Some(c) if c.is_alphabetic() || c == '_')
        && ch.all(|c| c.is_alphanumeric() || c == '_')
}

/// `rc, out = subprocess.run(...)` 这类解包赋值（run/check_output/check_call/call
/// 分别返回 CompletedProcess / bytes，不可按元组解包）。
fn is_unpack_assignment(t: &str) -> bool {
    let Some(eq) = t.find('=') else { return false };
    if t.as_bytes().get(eq + 1) == Some(&b'=') {
        return false; // `==` 是比较不是赋值
    }
    let lhs = t[..eq].trim();
    let rhs = t[eq + 1..].trim();
    let unpackable = rhs.starts_with("subprocess.run(")
        || rhs.starts_with("subprocess.check_output(")
        || rhs.starts_with("subprocess.check_call(")
        || rhs.starts_with("subprocess.call(");
    unpackable && lhs.contains(',') && lhs.split(',').map(str::trim).all(is_ident)
}

fn bare_except(t: &str) -> bool {
    t.strip_prefix("except")
        .is_some_and(|r| r.trim_start().starts_with(':'))
}

fn py_line(line: &str, n: usize, findings: &mut Vec<Finding>) {
    let t = line.trim_start();
    if is_unpack_assignment(t) {
        findings.push(err(
            "PY_UNPACK",
            "subprocess.run()/check_output() 返回 CompletedProcess/bytes，不能按元组解包 \
             （'rc, out = ...'）——用 p = subprocess.run(..., capture_output=True) 后取 \
             p.returncode/p.stdout/p.stderr"
                .to_string(),
            n,
        ));
    }
    if bare_except(t) {
        findings.push(warn(
            "PY_BARE_EXCEPT",
            "bare 'except:' 会连 KeyboardInterrupt/SystemExit 一起吞——用 'except Exception:' 并记录日志"
                .to_string(),
            n,
        ));
    }
    if t.contains("os.system(") {
        findings.push(warn(
            "PY_OS_SYSTEM",
            "os.system() 无超时、返回码易被忽略——用 subprocess.run([...], check=...)".to_string(),
            n,
        ));
    }
}

fn check_embedded_python(text: &str, toks: &[Token], findings: &mut Vec<Finding>) {
    // python -c '<code>'（tokenizer 已剥引号、保留换行）
    for w in toks.windows(3) {
        if (w[0].text == "python" || w[0].text == "python3") && w[1].text == "-c" {
            for (off, cl) in w[2].text.lines().enumerate() {
                py_line(cl, w[2].line + off, findings);
            }
        }
    }
    // python - <<EOF ... EOF（ponytail: 不处理嵌套 heredoc）
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        if is_python_cmd(lines[i])
            && let Some(delim) = heredoc_delim(lines[i])
        {
            let mut j = i + 1;
            while j < lines.len() && lines[j].trim() != delim {
                j += 1;
            }
            for (off, cl) in lines[i + 1..j.min(lines.len())].iter().enumerate() {
                py_line(cl, i + 2 + off, findings);
            }
            i = j;
        }
        i += 1;
    }
}

// ---- bash -n ----

/// `bash -n` 语法检查（整段命令串）。bash 不可用（Windows 无 Git Bash 等）→
/// 降级 info finding，不阻断（recoverable）。
fn check_bash_syntax(text: &str) -> Option<Finding> {
    if text.trim().is_empty() {
        return None;
    }
    let tmp = std::env::temp_dir().join(format!("serena-lint-shell-{}.sh", std::process::id()));
    if std::fs::write(&tmp, text).is_err() {
        return Some(info(
            "BASH_UNAVAILABLE",
            "cannot write temp file; skipped bash -n syntax check".to_string(),
            text.lines().count(),
        ));
    }
    // Git Bash 的 bash.exe 接受正斜杠 Windows 路径；反斜杠会被当转义。
    let tmp_str = tmp.to_string_lossy().replace('\\', "/");
    let out = std::process::Command::new("bash")
        .arg("-n")
        .arg(&tmp_str)
        .output();
    let _ = std::fs::remove_file(&tmp);
    match out {
        Err(_) => Some(info(
            "BASH_UNAVAILABLE",
            "bash -n not available; skipped bash syntax check".to_string(),
            text.lines().count(),
        )),
        Ok(o) if !o.status.success() => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            // Windows PATH 上的 bash 可能是 WSL stub（无发行版时 execvpe /bin/bash
            // 固定失败，stderr 以 "WSL (...)" 开头）——属"bash 调不到"，降级 info
            // 而非误报脚本语法错。
            if stderr.contains("WSL") {
                return Some(info(
                    "BASH_UNAVAILABLE",
                    "bash on PATH is a WSL stub without a distro; skipped bash syntax check"
                        .to_string(),
                    text.lines().count(),
                ));
            }
            let first = stderr.lines().next().unwrap_or("");
            // bash 自身的报错必以 "bash:" 开头（`bash: -c: line N: ...`）；空 stderr
            // 或非 bash 前缀（WSL stub 无输出现象、壳层包装错误）→ 无法归因脚本
            // 语法错，降级 info 而非误报 warning（CI windows runner 实锤）。
            if !first.starts_with("bash:") {
                return Some(info(
                    "BASH_UNAVAILABLE",
                    "bash -n failed without a bash-format error; skipped bash syntax check"
                        .to_string(),
                    text.lines().count(),
                ));
            }
            let first: String = first.chars().take(200).collect();
            // stderr 形如 `bash: line 5: syntax error...`，能拿到就报告真实行号。
            let line = extract_bash_line(&stderr).unwrap_or_else(|| text.lines().count());
            Some(warn("BASH_SYNTAX", format!("bash -n: {first}"), line))
        }
        Ok(_) => None,
    }
}

fn extract_bash_line(stderr: &str) -> Option<usize> {
    let idx = stderr.find("line ")?;
    let rest = &stderr[idx + 5..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

// ============== 入口 ==============

fn lint(text: &str) -> Vec<Finding> {
    let toks = tokenize(text);
    let mut findings = Vec::new();
    check_cli_calls(&toks, &mut findings);
    check_flag_lines(&toks, &mut findings);
    check_embedded_python(text, &toks, &mut findings);
    findings
}

fn render_json(findings: &[Finding]) -> String {
    let (mut e, mut w, mut i) = (0u64, 0u64, 0u64);
    let items: Vec<serde_json::Value> = findings
        .iter()
        .map(|f| {
            match f.severity {
                Severity::Error => e += 1,
                Severity::Warning => w += 1,
                Severity::Info => i += 1,
            }
            json!({
                "severity": f.severity.as_str(),
                "code": f.code,
                "message": f.message,
                "line": f.line,
            })
        })
        .collect();
    json!({
        "findings": items,
        "summary": {"errors": e, "warnings": w, "infos": i},
    })
    .to_string()
}

fn decide_exit(findings: &[Finding], strict: bool) -> u8 {
    if strict && findings.iter().any(|f| f.severity == Severity::Error) {
        2
    } else {
        0
    }
}

/// lint-shell 主入口（同步；由 main.rs 管理命令分支调用）。返回 exit code：
/// 默认 warn-only 恒 0；`--strict` 且存在 error 级 finding → 2。
pub(crate) fn run(text: &str, json_output: bool, strict: bool) -> u8 {
    let mut findings = lint(text);
    if let Some(f) = check_bash_syntax(text) {
        findings.push(f);
    }
    if json_output {
        println!("{}", render_json(&findings));
    } else {
        for f in &findings {
            println!(
                "[{}][{}] {} (line {})",
                f.severity.as_str(),
                f.code,
                f.message,
                f.line
            );
        }
    }
    decide_exit(&findings, strict)
}

// ============== 单测 ==============

#[cfg(test)]
mod tests {
    use super::*;

    fn codes(findings: &[Finding]) -> Vec<&'static str> {
        findings.iter().map(|f| f.code).collect()
    }

    #[test]
    fn tokenize_quotes_continuation_and_lines() {
        let toks = tokenize("cli def 'a b.rs' \\\n  1 2\nhover c.rs 3 4");
        let texts: Vec<&str> = toks.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(
            texts,
            ["cli", "def", "a b.rs", "1", "2", "hover", "c.rs", "3", "4"]
        );
        // 续行后的 token 记物理行 2。
        assert_eq!(toks[3].line, 2);
        assert_eq!(toks[5].line, 3);
        // 操作符切分 + 引号内换行保留。
        let toks2 = tokenize("ls && python -c 'x = 1\nos.system(\"ls\")'");
        let texts2: Vec<&str> = toks2.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(
            texts2,
            ["ls", "&", "&", "python", "-c", "x = 1\nos.system(\"ls\")"]
        );
    }

    /// 锁定手工清单与 clap 派生自 enum Cmd 的子命令名完全一致（双向）。
    #[test]
    fn tool_names_match_clap_enum() {
        use clap::CommandFactory;
        let cmd = crate::Cli::command();
        let clap_names: std::collections::BTreeSet<&str> =
            cmd.get_subcommands().map(|c| c.get_name()).collect();
        let mine: std::collections::BTreeSet<&str> = TOOL_NAMES.iter().copied().collect();
        assert_eq!(
            mine, clap_names,
            "TOOL_NAMES 与 enum Cmd drift（见 main.rs enum Cmd）"
        );
    }

    #[test]
    fn position_shapes_consistent() {
        assert!(!POSITION_TOOLS.is_empty());
        assert!(!PATH_FIRST_TOOLS.is_empty());
        for s in POSITION_TOOLS {
            assert!(TOOL_NAMES.contains(&s.tool), "{} not in TOOL_NAMES", s.tool);
            assert!(!s.line_args.is_empty());
            let max = *s.line_args.iter().max().unwrap();
            assert!(
                max < s.arity,
                "{}: line idx {max} >= arity {}",
                s.tool,
                s.arity
            );
        }
        for t in PATH_FIRST_TOOLS {
            assert!(TOOL_NAMES.contains(t), "{t} not in TOOL_NAMES");
        }
    }

    #[test]
    fn good_call_no_findings() {
        let f = lint("serena-cli find-symbol foo");
        assert!(f.is_empty(), "{f:?}");
    }

    #[test]
    fn unknown_tool_error() {
        let f = lint("serena-cli find-sympol x");
        assert_eq!(codes(&f), ["UNKNOWN_TOOL"]);
        assert_eq!(f[0].severity, Severity::Error);
        assert_eq!(f[0].line, 1);
    }

    #[test]
    fn line_zero_is_1based_error() {
        let f = lint("cli.exe rename-symbol Cargo.toml 0 5 --to X");
        assert_eq!(codes(&f), ["LINE_NOT_1BASED"]);
        assert_eq!(f[0].severity, Severity::Error);
    }

    #[test]
    fn missing_line_args_warn() {
        let f = lint("cli def src/main.rs");
        assert_eq!(codes(&f), ["LINE_MISSING"]);
        assert_eq!(f[0].severity, Severity::Warning);
    }

    #[test]
    fn missing_path_warns_existing_does_not() {
        let f = lint("cli.exe def definitely_missing_12345.rs 1 2");
        assert_eq!(codes(&f), ["PATH_MISSING"]);
        assert_eq!(f[0].severity, Severity::Warning);
        // cwd = crates/cli（package 目录），Cargo.toml 必存在。
        let f2 = lint("cli.exe def Cargo.toml 1 2");
        assert!(!codes(&f2).contains(&"PATH_MISSING"), "{f2:?}");
    }

    #[test]
    fn heredoc_unpack_reports_line_number() {
        let cmd = "echo start\npython - <<EOF\nrc, out = subprocess.run(['ls'])\nprint(rc)\nEOF";
        let f = lint(cmd);
        assert_eq!(codes(&f), ["PY_UNPACK"]);
        assert_eq!(f[0].line, 3);
    }

    #[test]
    fn dash_c_os_system_and_bare_except() {
        let cmd = "python -c 'import os\ntry:\n  os.system(\"ls\")\nexcept:\n  pass'";
        let f = lint(cmd);
        assert_eq!(codes(&f), ["PY_OS_SYSTEM", "PY_BARE_EXCEPT"]);
        assert_eq!(f[0].severity, Severity::Warning);
        assert_eq!(f[1].severity, Severity::Warning);
    }

    #[test]
    fn correct_tuple_unpack_not_flagged() {
        let cmd = "python - <<EOF\np = subprocess.run(['ls'], capture_output=True)\nrc = p.returncode\nEOF";
        let f = lint(cmd);
        assert!(f.is_empty(), "{f:?}");
    }

    #[test]
    fn flag_line_zero_error() {
        let f = lint("cli.exe read-file Cargo.toml --start-line 0");
        assert_eq!(codes(&f), ["LINE_NOT_1BASED"]);
        let f2 = lint("cli.exe read-file Cargo.toml --end-line=0");
        assert_eq!(codes(&f2), ["LINE_NOT_1BASED"]);
    }

    #[test]
    fn decide_exit_strict_only_on_error() {
        let warns = vec![warn("PATH_MISSING", "x".into(), 1)];
        assert_eq!(decide_exit(&warns, true), 0);
        assert_eq!(decide_exit(&warns, false), 0);
        let errs = vec![err("UNKNOWN_TOOL", "x".into(), 1)];
        assert_eq!(decide_exit(&errs, true), 2);
        assert_eq!(decide_exit(&errs, false), 0);
    }

    #[test]
    fn render_json_shape() {
        let findings = vec![err("UNKNOWN_TOOL", "bad tool".into(), 1)];
        let v: serde_json::Value = serde_json::from_str(&render_json(&findings)).unwrap();
        assert_eq!(v["findings"][0]["severity"], "error");
        assert_eq!(v["findings"][0]["code"], "UNKNOWN_TOOL");
        assert_eq!(v["findings"][0]["line"], 1);
        assert_eq!(v["summary"]["errors"], 1);
        assert_eq!(v["summary"]["warnings"], 0);
    }
}
