//! LSP `Position` ↔ 字节偏移的双向换算（PLAN Task 8 LF-1 / ARCHITECTURE §3.2）。
//!
//! `Session` 协商时声明 `general.position_encodings = [utf-16, utf-8]`（见 `init_params`）；
//! 服务器在 `initialize` 响应中通过 `capabilities.positionEncoding` 选定其一，supervisor
//! 据此构造 `OffsetEncoding` 并在后续 `textDocument/*` 请求的 range/position 中贯穿使用。
//! Task 10 的 def/refs 直传与 Task 15 的 range 切片必须经此模块换算，禁裸算（PLAN §3.2）。
//!
//! 实现要点：
//! - LSP `Position.character` 是**单元计数**（utf-8/16 code unit，**非字节**）。
//!   `byte ↔ position` 必须经过 UTF-8 字节 ↔ char 计数 ↔ unit 计数 的两步映射；
//!   因此 utf-8 时 byte = char_count（一一对应：每个 char 1 字节），utf-16 时
//!   byte ≠ char_count（BMP 外字符 4 字节 vs 2 code unit）。
//! - utf-32 暂未实现：返回 `OffsetError::Unsupported`，留待有 server 真选 utf-32 时补。
//! - 行/列双双越界（line 超过末行；或 character 超过该行字符数）→ `OutOfRange`。
//!
//! ponylabel: 线性扫描未建索引，单次文本通常 <1MB 毫秒级，开销远低于一次 LSP 往返；
//! 若实测大文件切片成为热点，可加 mmap+二分线表（推迟到 M3+）。

use std::fmt;

/// 选定的 LSP `PositionEncodingKind`。
///
/// 与 `lsp_types::PositionEncodingKind` 的差异：本模块是协议无关的纯枚举，
/// 转换放在 `from_lsp_kind` 里（lsp-types → 本枚举）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OffsetEncoding {
    Utf8,
    Utf16,
    /// 当前未实现 —— 调用方应准备好看到 `OffsetError::Unsupported`。
    Utf32,
}

impl OffsetEncoding {
    /// 服务器在 `capabilities.positionEncoding` 选定的字符串（lsp-types 提供的常量）。
    /// 未知值按 `Utf16` 处理 —— 那是 LSP 历史默认，比乱报 unsupported 更友好。
    pub fn from_lsp_kind(kind: &lsp_types::PositionEncodingKind) -> Self {
        if kind == &lsp_types::PositionEncodingKind::UTF8 {
            Self::Utf8
        } else if kind == &lsp_types::PositionEncodingKind::UTF32 {
            Self::Utf32
        } else {
            // UTF16 / UTF16_DEFAULT_LIKE / 未知
            Self::Utf16
        }
    }
}

impl fmt::Display for OffsetEncoding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Utf8 => f.write_str("utf-8"),
            Self::Utf16 => f.write_str("utf-16"),
            Self::Utf32 => f.write_str("utf-32"),
        }
    }
}

/// 换算错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OffsetError {
    /// 编码未实现（当前只有 `Utf32`）。
    Unsupported,
    /// 位置越界（line 超过末行 / character 超过该行字符数）。
    OutOfRange,
}

impl fmt::Display for OffsetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported => f.write_str("offset encoding not supported"),
            Self::OutOfRange => f.write_str("position out of range"),
        }
    }
}

impl std::error::Error for OffsetError {}

pub type Result<T, E = OffsetError> = std::result::Result<T, E>;

/// LSP `Position`：line/character 按行/编码 unit 计数（与 lsp_types::Position 字段相同，
/// 这里独立定义避免 lsp-types 依赖蔓延；supervisor 处做转换）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

/// 计算文本中各行的字节范围（半开区间 `[start, end)` —— 不含行尾 `\n`）。
fn line_byte_ranges(text: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut line_begin = 0usize;
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            ranges.push((line_begin, i));
            line_begin = i + 1;
        }
    }
    if line_begin <= text.len() {
        ranges.push((line_begin, text.len()));
    }
    ranges
}

/// 给定一个编码 + 行字节切片 + character（code unit 计数）→ 行内字节偏移。
/// character 越界或编码不支持 → `None`。
fn unit_to_byte_in_line(line_bytes: &[u8], character: u32, enc: OffsetEncoding) -> Option<usize> {
    let s = std::str::from_utf8(line_bytes).ok()?;
    let mut count: u32 = 0;
    for (char_byte_start, ch) in s.char_indices() {
        let units_per_char: u32 = match enc {
            OffsetEncoding::Utf8 => ch.len_utf8() as u32,
            OffsetEncoding::Utf16 => {
                if ch.len_utf16() == 1 {
                    1
                } else {
                    2
                }
            }
            OffsetEncoding::Utf32 => return None,
        };
        let next = count + units_per_char;
        if character < next {
            // 落入当前字符内部；按起始字节算（与 clangd 行为对齐 —— 选择字符起始而非字节中间）。
            return Some(char_byte_start);
        }
        count = next;
        if character == count {
            // 恰好落到字符末尾之后 → 该字符结尾字节索引（不含 trailing UTF-8 字节）。
            return Some(char_byte_start + ch.len_utf8());
        }
    }
    None
}

/// 给定编码 + 行字节切片 + 行内字节偏移 → character（code unit 计数）。
fn byte_to_unit_in_line(
    line_bytes: &[u8],
    byte_offset_in_line: usize,
    enc: OffsetEncoding,
) -> Option<u32> {
    let s = std::str::from_utf8(line_bytes).ok()?;
    let mut character: u32 = 0;
    for (char_byte_start, ch) in s.char_indices() {
        let char_byte_end = char_byte_start + ch.len_utf8();
        let char_units: u32 = match enc {
            OffsetEncoding::Utf8 => ch.len_utf8() as u32,
            OffsetEncoding::Utf16 => {
                if ch.len_utf16() == 1 {
                    1
                } else {
                    2
                }
            }
            OffsetEncoding::Utf32 => return None,
        };
        if byte_offset_in_line <= char_byte_start {
            return Some(character);
        }
        // 严格小于 char_byte_end：落入字符内部 → 按字符起始计。
        if byte_offset_in_line < char_byte_end {
            return Some(character);
        }
        // 等于 char_byte_end：字符计入后再返回，确保与正向 `unit_to_byte_in_line` 的
        // `character == count → 返回 char_byte_end` 一致。
        character += char_units;
        if byte_offset_in_line == char_byte_end {
            return Some(character);
        }
    }
    Some(character)
}

/// Position → 字节偏移（文本起点到该 position 的字节数）。
///
/// `Position.character` 按编码 unit 计数（与 LSP 协议定义一致）。本函数是该方向的权威实现。
pub fn position_to_byte(text: &str, pos: Position, enc: OffsetEncoding) -> Result<usize> {
    if matches!(enc, OffsetEncoding::Utf32) {
        return Err(OffsetError::Unsupported);
    }
    let ranges = line_byte_ranges(text);
    let line_idx = pos.line as usize;
    let (line_start, line_end) = *ranges.get(line_idx).ok_or(OffsetError::OutOfRange)?;
    let line_bytes = &text.as_bytes()[line_start..line_end];
    let byte_in_line =
        unit_to_byte_in_line(line_bytes, pos.character, enc).ok_or(OffsetError::OutOfRange)?;
    Ok(line_start + byte_in_line)
}

/// 字节偏移 → Position。
pub fn byte_to_position(text: &str, byte: usize, enc: OffsetEncoding) -> Result<Position> {
    if matches!(enc, OffsetEncoding::Utf32) {
        return Err(OffsetError::Unsupported);
    }
    if byte > text.len() {
        return Err(OffsetError::OutOfRange);
    }
    let ranges = line_byte_ranges(text);
    for (line_idx, (line_start, line_end)) in ranges.iter().enumerate() {
        if byte >= *line_start && byte <= *line_end {
            let line_bytes = &text.as_bytes()[*line_start..*line_end];
            let byte_in_line = byte - *line_start;
            let character = byte_to_unit_in_line(line_bytes, byte_in_line, enc)
                .ok_or(OffsetError::OutOfRange)?;
            return Ok(Position {
                line: line_idx as u32,
                character,
            });
        }
    }
    Err(OffsetError::OutOfRange)
}

/// 半开区间 `[start, end)` 切片。两端必须落在 char 边界（不切字符内部）。
///
/// 错误码：编码不支持 → `Unsupported`；位置越界 → `OutOfRange`。
pub fn slice_at(text: &str, start: Position, end: Position, enc: OffsetEncoding) -> Result<String> {
    let start_byte = position_to_byte(text, start, enc)?;
    let end_byte = position_to_byte(text, end, enc)?;
    if start_byte > end_byte {
        return Err(OffsetError::OutOfRange);
    }
    // 边界必须在 char 边界 —— 否则切碎字符。`str::get` 自动校验 char 边界。
    text.get(start_byte..end_byte)
        .map(str::to_string)
        .ok_or(OffsetError::OutOfRange)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_lsp_kind_handles_known_and_default() {
        assert_eq!(
            OffsetEncoding::from_lsp_kind(&lsp_types::PositionEncodingKind::UTF8),
            OffsetEncoding::Utf8
        );
        assert_eq!(
            OffsetEncoding::from_lsp_kind(&lsp_types::PositionEncodingKind::UTF16),
            OffsetEncoding::Utf16
        );
        assert_eq!(
            OffsetEncoding::from_lsp_kind(&lsp_types::PositionEncodingKind::UTF32),
            OffsetEncoding::Utf32
        );
    }

    #[test]
    fn line_byte_ranges_basic() {
        let r = line_byte_ranges("a\nb\n");
        assert_eq!(r, vec![(0, 1), (2, 3), (4, 4)]);
        let r = line_byte_ranges("abc");
        assert_eq!(r, vec![(0, 3)]);
    }
}
