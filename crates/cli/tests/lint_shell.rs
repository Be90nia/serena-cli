//! lint-shell 子命令端到端测试（bd serena-rust-8ot 验收 2-6）。
//!
//! 走真实二进制（CARGO_BIN_EXE_serena-cli），覆盖 exit code 协议与输出格式。

use std::process::Command;

fn lint_cmd(cmd: &str, extra: &[&str]) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_serena-cli"))
        .args(["lint-shell", "--cmd", cmd])
        .args(extra)
        .output()
        .expect("spawn cli");
    (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stdout).into_owned())
}

/// 验收 2：好命令零 error/warning → exit 0（bash -n 不可用时允许 info 级输出）。
#[test]
fn good_tool_is_silent_and_passes() {
    let (rc, out) = lint_cmd("serena-cli find-symbol foo", &[]);
    assert_eq!(rc, 0);
    assert!(!out.contains("[error]") && !out.contains("[warning]"), "got: {out}");
}

/// 验收 3：错工具名 → warn-only exit 0 + [error][UNKNOWN_TOOL]；--strict → exit 2。
#[test]
fn unknown_tool_warn_only_then_strict_exit_2() {
    let (rc, out) = lint_cmd("serena-cli find-sympol x", &[]);
    assert_eq!(rc, 0, "warn-only must stay 0");
    assert!(out.contains("[error][UNKNOWN_TOOL]"), "got: {out}");
    let (rc_strict, _) = lint_cmd("serena-cli find-sympol x", &["--strict"]);
    assert_eq!(rc_strict, 2);
}

/// 验收 4：line=0 → [error] 1-based finding，exit 0。
#[test]
fn line_zero_reports_1based() {
    let (rc, out) = lint_cmd("cli.exe rename-symbol f.rs 0 5 --to X", &[]);
    assert_eq!(rc, 0);
    assert!(out.contains("LINE_NOT_1BASED"), "got: {out}");
    assert!(out.contains("1-based"), "got: {out}");
    assert!(out.contains("[error]"), "got: {out}");
}

/// 验收 5：heredoc 内 subprocess 二元解包 → PY_UNPACK + 正确行号。
#[test]
fn heredoc_unpack_reports_line_number() {
    let cmd = "python - <<EOF\nrc, out = subprocess.run(['ls'])\nEOF";
    let (rc, out) = lint_cmd(cmd, &[]);
    assert_eq!(rc, 0);
    assert!(out.contains("PY_UNPACK"), "got: {out}");
    assert!(out.contains("subprocess"), "got: {out}");
    assert!(out.contains("line 2"), "got: {out}");
}

/// 验收 6：--json 输出合法 JSON（findings + summary）。
#[test]
fn json_output_is_valid() {
    let (rc, out) = lint_cmd("serena-cli find-sympol x", &["--json"]);
    assert_eq!(rc, 0);
    let v: serde_json::Value = serde_json::from_str(out.trim()).expect("valid json");
    assert!(v["findings"].is_array(), "got: {v}");
    assert_eq!(v["findings"][0]["code"], "UNKNOWN_TOOL");
    assert_eq!(v["findings"][0]["severity"], "error");
    assert_eq!(v["summary"]["errors"], 1);
    assert_eq!(v["summary"]["warnings"], 0);
}

/// stdin 路径：--cmd-stdin 等价 --cmd。
#[test]
fn cmd_stdin_equivalent() {
    use std::io::Write;
    let mut child = Command::new(env!("CARGO_BIN_EXE_serena-cli"))
        .args(["lint-shell", "--cmd-stdin", "--strict"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("spawn cli");
    child.stdin.as_mut().expect("stdin").write_all(b"serena-cli find-sympol x").expect("write");
    let out = child.wait_with_output().expect("wait");
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stdout).contains("UNKNOWN_TOOL"));
}
