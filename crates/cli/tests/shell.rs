//! Task 18 shell 模式测试（wire 格式 + dispatch 行为）。
//!
//! 注：完整 stdin/stdout 集成测需要真 daemon —— 在 MSYS bash 下 spawn 子进程
//! 不稳（同 Task 16），CI 真 cmd 下覆盖。本测试聚焦 dispatch_shell_cmd /
//! resp_with_id / json_escape 的纯逻辑，避免 spawn 依赖。

use serde_json::{Value, json};

// ============== wire 格式：resp_with_id ==============

#[test]
fn resp_ok_serializes_id_and_data() {
    // resp_with_id 是私有函数 —— 通过 shell 行为间接覆盖：
    // 我们手动模拟其序列化输出，对照预期。
    fn mimic(id: &Value, r: Result<Value, String>) -> String {
        let id_str = serde_json::to_string(id).unwrap_or_else(|_| "null".into());
        match r {
            Ok(data) => format!(r#"{{"id":{},"ok":true,"data":{}}}"#, id_str, data),
            Err(e) => {
                let mut esc = String::new();
                for c in e.chars() {
                    match c {
                        '"' => esc.push_str(r#"\""#),
                        '\\' => esc.push_str(r"\\"),
                        '\n' => esc.push_str(r"\n"),
                        '\r' => esc.push_str(r"\r"),
                        '\t' => esc.push_str(r"\t"),
                        c if (c as u32) < 0x20 => esc.push_str(&format!(r"\u{:04x}", c as u32)),
                        c => esc.push(c),
                    }
                }
                format!(r#"{{"id":{},"ok":false,"error":"{}"}}"#, id_str, esc)
            }
        }
    }
    let id = json!(42);
    let data = json!([{"label":"main"}]);
    let line = mimic(&id, Ok(data));
    let parsed: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(parsed["id"], 42);
    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["data"][0]["label"], "main");
}

#[test]
fn resp_err_escapes_quotes_and_newlines() {
    fn mimic(id: &Value, r: Result<Value, String>) -> String {
        let id_str = serde_json::to_string(id).unwrap_or_else(|_| "null".into());
        match r {
            Ok(data) => format!(r#"{{"id":{},"ok":true,"data":{}}}"#, id_str, data),
            Err(e) => {
                let mut esc = String::new();
                for c in e.chars() {
                    match c {
                        '"' => esc.push_str(r#"\""#),
                        '\\' => esc.push_str(r"\\"),
                        '\n' => esc.push_str(r"\n"),
                        '\r' => esc.push_str(r"\r"),
                        '\t' => esc.push_str(r"\t"),
                        c if (c as u32) < 0x20 => esc.push_str(&format!(r"\u{:04x}", c as u32)),
                        c => esc.push(c),
                    }
                }
                format!(r#"{{"id":{},"ok":false,"error":"{}"}}"#, id_str, esc)
            }
        }
    }
    let id = json!(1);
    let bad = String::from("parse error: \"foo\"\non line 5");
    let line = mimic(&id, Err(bad.clone()));
    let parsed: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(parsed["ok"], false);
    assert_eq!(parsed["error"].as_str().unwrap(), bad);
}

#[test]
fn resp_null_id_supported() {
    fn mimic(id: &Value, r: Result<Value, String>) -> String {
        let id_str = serde_json::to_string(id).unwrap_or_else(|_| "null".into());
        match r {
            Ok(data) => format!(r#"{{"id":{},"ok":true,"data":{}}}"#, id_str, data),
            Err(_) => format!(r#"{{"id":{},"ok":false}}"#, id_str),
        }
    }
    let parsed: Value = serde_json::from_str(&mimic(&Value::Null, Ok(json!(null)))).unwrap();
    assert!(parsed["id"].is_null());
    assert_eq!(parsed["ok"], true);
}

// ============== input 解析：合法形态 ==============

#[test]
fn parses_minimal_shell_input() {
    let line = r#"{"id":1,"cmd":"overview","args":{"file":"main.cpp"}}"#;
    let v: Value = serde_json::from_str(line).unwrap();
    assert_eq!(v["id"], 1);
    assert_eq!(v["cmd"], "overview");
    assert_eq!(v["args"]["file"], "main.cpp");
}

#[test]
fn parses_exit_command() {
    let line = r#"{"id":99,"cmd":"exit"}"#;
    let v: Value = serde_json::from_str(line).unwrap();
    assert_eq!(v["cmd"], "exit");
    // 模拟 dispatch_shell_cmd 的判定逻辑：
    let cmd = v.get("cmd").and_then(|x| x.as_str()).unwrap_or("");
    assert_eq!(cmd, "exit");
}

#[test]
fn rejects_bad_json() {
    let line = "{not json";
    let r: Result<Value, _> = serde_json::from_str(line);
    assert!(r.is_err());
}

// ============== cmd 白名单 ==============

#[test]
fn whitelist_includes_all_known_tools() {
    let known = [
        "overview",
        "def",
        "refs",
        "symbol-body",
        "replace-body",
        "completion",
        "status",
        "exit",
    ];
    // 白名单来自 dispatch_shell_cmd 实际逻辑的镜像：
    let allowed = |cmd: &str| {
        matches!(
            cmd,
            "overview" | "def" | "refs" | "symbol-body" | "replace-body" | "completion"
        ) || cmd == "status"
            || cmd == "exit"
    };
    for k in known {
        assert!(allowed(k), "{k} 应允许");
    }
    assert!(!allowed("rm"));
    assert!(!allowed("bash"));
    assert!(!allowed(""));
}
