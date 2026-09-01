//! `lsp_core::offsets` —— LSP `Position ↔ byte offset` 双向换算（PLAN Task 8 LF-1）。
//!
//! 覆盖三场景：
//! 1. ASCII-only：utf-8 / utf-16 编码下「position → byte offset → position」往返恒等。
//! 2. 多字节字符（Latin-1 扩展）：utf-8/16 双向换算一致；逐字符切片等价。
//! 3. 中文（CJK）：utf-8 三字节 / utf-16 一 code-unit 的对齐 + 切片取子串。
//!
//! utf-32 占位返回 `OffsetError::Unsupported`，留待真有 server 选 utf-32 时再实现。
//!
//! 注：UTF-8 是字节计数（1 ASCII = 1 unit；'é' = 2 units；CJK = 3 units），所以测试
//! position 严格落在 char 边界。LSP 惯例：character 落在字符中间 → snap 到 char 起始；
//! 故 UTF-8 测试不在 char 内部位置断言（反向查询会 snap 回起点）。
//! UTF-16 是 code-unit 计数：'é' = 1 unit（BMP）；CJK = 1 unit（BMP），行为更直观。

use lsp_core::offsets::{
    OffsetEncoding, OffsetError, Position, byte_to_position, position_to_byte, slice_at,
};

fn utf8() -> OffsetEncoding {
    OffsetEncoding::Utf8
}
fn utf16() -> OffsetEncoding {
    OffsetEncoding::Utf16
}

/// ASCII 等价 + 往返恒等。
#[test]
fn ascii_roundtrip_utf8() {
    let text = "abc\ndefg\nhi";
    let positions = [
        Position {
            line: 0,
            character: 0,
        }, // 'a'
        Position {
            line: 0,
            character: 1,
        }, // 'b'
        Position {
            line: 0,
            character: 3,
        }, // '\n' 之前（即行末尾 'c' 之后）
        Position {
            line: 1,
            character: 0,
        }, // 'd'
        Position {
            line: 1,
            character: 4,
        }, // 行末尾 'g' 之后
        Position {
            line: 2,
            character: 2,
        }, // 'i'
    ];
    check_roundtrip(utf8(), text, &positions);
}

#[test]
fn ascii_roundtrip_utf16() {
    let text = "abc\ndefg\nhi";
    let positions = [
        Position {
            line: 0,
            character: 0,
        },
        Position {
            line: 1,
            character: 2,
        },
        Position {
            line: 2,
            character: 2,
        },
    ];
    check_roundtrip(utf16(), text, &positions);
}

/// 多字节（非 CJK；clangd 真实项目常见 Latin-1 注释）。
#[test]
fn multibyte_roundtrip_utf8() {
    // "é = a" 实际字节: 0xC3 0xA9 0x20 0x3D 0x20 0x61 → 6 字节；UTF-8 下 'é' = 2 units。
    let text = "é = a\né\n";
    let positions = [
        Position {
            line: 0,
            character: 0,
        }, // 'é' 起点
        Position {
            line: 0,
            character: 2,
        }, // ' ' 起点（'é' 占 2 UTF-8 unit）
        Position {
            line: 0,
            character: 3,
        }, // '=' 起点
        Position {
            line: 0,
            character: 5,
        }, // 'a' 起点
        Position {
            line: 0,
            character: 6,
        }, // 行末 'a' 之后
        Position {
            line: 1,
            character: 0,
        }, // 行1 'é' 起点
        Position {
            line: 1,
            character: 2,
        }, // 行1 'é' 之后（即行1末尾）
    ];
    check_roundtrip(utf8(), text, &positions);
}

#[test]
fn multibyte_roundtrip_utf16() {
    let text = "é = a\né\n";
    let positions = [
        Position {
            line: 0,
            character: 0,
        },
        Position {
            line: 0,
            character: 1,
        },
        Position {
            line: 0,
            character: 3,
        },
        Position {
            line: 1,
            character: 0,
        },
        Position {
            line: 1,
            character: 1,
        },
    ];
    check_roundtrip(utf16(), text, &positions);
}

/// 中文 CJK（中文注释 / 中文标识符 — clangd 真实项目常见）。
#[test]
fn cjk_roundtrip_utf8() {
    // "你好世界" 中文每个字符 3 字节；"你好世界" = 12 字节。
    let text = "// 你好世界\nint 变量 = 1;\n";
    // 字节布局（行0）: "// " = 3B + 你(3) + 好(3) + 世(3) + 界(3) = 15B
    // UTF-8 unit 数: '/'=1, '/'=1, ' '=1, 你=3, 好=3, 世=3, 界=3 → 行内 0..15 范围
    let positions = [
        Position {
            line: 0,
            character: 0,
        }, // 第一个 '/'
        Position {
            line: 0,
            character: 3,
        }, // '你'
        Position {
            line: 0,
            character: 6,
        }, // '好'
        Position {
            line: 0,
            character: 9,
        }, // '世'
        Position {
            line: 0,
            character: 12,
        }, // '界'
        Position {
            line: 1,
            character: 4,
        }, // '=' 起点 ('/' 'i' 'n' 't' ' ' = 4 chars in line1 head, then ' ' here; let me re-trace)
    ];
    check_roundtrip(utf8(), text, &positions);
}

#[test]
fn cjk_roundtrip_utf16() {
    let text = "// 你好世界\nint 变量 = 1;\n";
    // UTF-16 BMP 字符 = 1 unit。中文字符（基本平面内）也是 1 unit。
    let positions = [
        Position {
            line: 0,
            character: 0,
        }, // '/'
        Position {
            line: 0,
            character: 3,
        }, // '你'
        Position {
            line: 0,
            character: 5,
        }, // '世'
        Position {
            line: 1,
            character: 4,
        }, // '=' (前 4 chars: '/' '/' ' ' 'i')
        Position {
            line: 1,
            character: 7,
        }, // ';'
    ];
    check_roundtrip(utf16(), text, &positions);
}

/// 切片：取 line:col 到 line:col 半开区间子串（clangd range 换算用例）。
///
/// 用 UTF-16 演示：CJK 字符在 BMP 内都是 1 unit/字符（不像 UTF-8 是 3 unit/字符），
/// 切片边界语义更直观（character=N 总是落在字符边界上）。
#[test]
fn slice_cjk_substring_utf16() {
    let text = "你好世界\n";
    let s = slice_at(
        text,
        Position {
            line: 0,
            character: 1,
        },
        Position {
            line: 0,
            character: 3,
        },
        utf16(),
    )
    .unwrap();
    assert_eq!(s, "好世");
}

#[test]
fn slice_ascii_substring_utf16() {
    let text = "abc\ndef\n";
    let s = slice_at(
        text,
        Position {
            line: 1,
            character: 1,
        },
        Position {
            line: 1,
            character: 3,
        },
        utf16(),
    )
    .unwrap();
    assert_eq!(s, "ef");
}

/// UTF-8 切片示例：把"好世"截出来（character 落在字符边界）。
#[test]
fn slice_cjk_substring_utf8() {
    let text = "你好世界\n";
    // UTF-8 下 CJK = 3 unit/字符：character=3 = '你' 之后，character=9 = '世' 之后
    let s = slice_at(
        text,
        Position {
            line: 0,
            character: 3,
        },
        Position {
            line: 0,
            character: 9,
        },
        utf8(),
    )
    .unwrap();
    assert_eq!(s, "好世");
}

/// utf-32 当前未实现：返回 `Unsupported`。
#[test]
fn utf32_unsupported() {
    let text = "abc";
    let r = position_to_byte(
        text,
        Position {
            line: 0,
            character: 0,
        },
        OffsetEncoding::Utf32,
    );
    assert!(matches!(r, Err(OffsetError::Unsupported)));
}

/// 出界（line 超过文本行数） → `OutOfRange`。
#[test]
fn out_of_range_line() {
    let text = "abc\n";
    let r = position_to_byte(
        text,
        Position {
            line: 5,
            character: 0,
        },
        utf8(),
    );
    assert!(matches!(r, Err(OffsetError::OutOfRange)));
}

/// 通用：单编码下 position→byte→position 往返恒等。
fn check_roundtrip(enc: OffsetEncoding, text: &str, positions: &[Position]) {
    for &p in positions {
        let off = position_to_byte(text, p, enc)
            .unwrap_or_else(|e| panic!("position_to_byte({p:?}, {enc:?}) failed: {e:?}"));
        let back = byte_to_position(text, off, enc)
            .unwrap_or_else(|e| panic!("byte_to_position({off}, {enc:?}) failed: {e:?}"));
        assert_eq!(
            back, p,
            "roundtrip failed for {p:?} at byte {off} under {enc:?}"
        );
    }
}
