//! bd fdmj-F3/F10 真机流方向契约：错误 JSON 统一走 **stdout**（成功载荷同流），
//! stderr 只有人读文本（clap usage / `[error]` 行）。流方向是全局 stdout 的
//! 进程属性，进程内断言不了——用 CARGO_BIN_EXE 进程外断言（真流方向）。

use std::process::Command;

fn run(args: &[&str]) -> (String, String, Option<i32>) {
    let out = Command::new(env!("CARGO_BIN_EXE_serena-cli"))
        .args(args)
        .output()
        .expect("spawn serena-cli");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code(),
    )
}

fn assert_bare_json_on_stdout(stdout: &str, stderr: &str) -> serde_json::Value {
    let trimmed = stdout.trim();
    assert!(
        !trimmed.contains('\n'),
        "stdout must be a single JSON line, got: {stdout:?}"
    );
    let v: serde_json::Value = serde_json::from_str(trimmed)
        .unwrap_or_else(|e| panic!("stdout must be bare JSON ({e}), got: {stdout:?}"));
    assert_eq!(v["code"], "BAD_ARGS", "unexpected error object: {v}");
    assert_eq!(v["retryable"], false);
    assert!(
        !stderr.contains("\"code\""),
        "stderr must not carry JSON, got: {stderr}"
    );
    v
}

/// clap 解析错（缺位置参数）→ JSON 在 stdout，usage 留 stderr，rc=2。
#[test]
fn clap_bad_args_json_goes_to_stdout_not_stderr() {
    let (stdout, stderr, code) = run(&["read-file"]);
    assert_eq!(code, Some(2), "rc=2 用法错语义不变");
    assert_bare_json_on_stdout(&stdout, &stderr);
    assert!(
        stderr.contains("Usage") || stderr.contains("error:"),
        "stderr keeps clap usage text, got: {stderr:?}"
    );
}

/// 多余位置参数（critic3-F3 复现形态：search 多词）→ 同流同形。
#[test]
fn extra_positional_bad_args_json_goes_to_stdout() {
    let (stdout, stderr, code) = run(&["search", "fn", "add"]);
    assert_eq!(code, Some(2));
    assert_bare_json_on_stdout(&stdout, &stderr);
}

/// 客户端侧校验（bad_args_exit 直调路径）→ 与 clap 路径同流同形。
#[test]
fn client_side_validation_json_goes_to_stdout() {
    let (stdout, stderr, code) = run(&["--max-tokens", "0", "overview", "x.py"]);
    assert_eq!(code, Some(2));
    let v = assert_bare_json_on_stdout(&stdout, &stderr);
    assert!(
        v["message"].as_str().unwrap_or("").contains("--max-tokens"),
        "message carries the offending flag detail: {v}"
    );
}

/// help/version 不走 JSON 路径（既有契约，防回归）。
#[test]
fn help_still_renders_natively_with_success() {
    let (stdout, stderr, code) = run(&["--help"]);
    assert_eq!(code, Some(0));
    assert!(stdout.contains("Usage"), "help renders to stdout");
    assert!(!stderr.contains("\"code\""));
}
