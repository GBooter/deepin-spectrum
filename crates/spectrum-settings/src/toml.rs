//! 极简 TOML 子集编解码（零依赖）。
//!
//! ## 为什么手写而不是引 `toml` crate
//!
//! 见 crate 文档与 `docs/设置与热重载设计.md` §2。一句话：本项目默认构建零外部
//! 依赖，而配置只需要「一层 section + 标量 + 字符串数组」这一小块语法。
//!
//! ## 支持的语法（**有意**的封闭子集）
//!
//! | 特性 | 状态 |
//! |---|---|
//! | `#` 行注释 | ✅ |
//! | `[section]`（单层） | ✅ |
//! | `key = value`（裸键 / 引号键） | ✅ |
//! | 布尔 / 整数 / 浮点 / 基本字符串 / 字面字符串 | ✅ |
//! | 数组（可跨行、允许尾逗号） | ✅ |
//! | `[[数组表]]` / 嵌套表 / 点号键 / 日期时间 / 多行字符串 | ❌ 明确报错 |
//!
//! 不支持的写法一律**报错而不是猜**：热重载最怕"看起来生效了、其实只解析了半条"。
//!
//! ## 容错边界（与热重载直接相关）
//!
//! - 文件被编辑器写到一半 → 解析失败 → 调用方保留**上一次的好配置**
//!   （[`crate::store::ConfigStore`] 负责），daemon 不会因为半行 TOML 挂掉；
//! - BOM、CRLF、行尾多余空白 → 容忍；
//! - 同一 section 重复出现 → 合并（手改配置时很常见，不视为错误）；
//! - 同一 section 内重复键 → **报错**（两份不同的值谁赢都是灾难）。

use std::fmt;

/// TOML 标量/数组值。
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// `true` / `false`。
    Bool(bool),
    /// 整数。
    Int(i64),
    /// 浮点。
    Float(f64),
    /// 字符串（基本或字面）。
    Str(String),
    /// 数组（可嵌套，本项目的 schema 只用一维字符串数组）。
    Array(Vec<Value>),
}

impl Value {
    /// 取布尔值。
    #[must_use]
    pub const fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(v) => Some(*v),
            _ => None,
        }
    }

    /// 取整数（浮点不做隐式取整，避免 `1.9` 悄悄变成 `1`）。
    #[must_use]
    pub const fn as_int(&self) -> Option<i64> {
        match self {
            Self::Int(v) => Some(*v),
            _ => None,
        }
    }

    /// 取浮点；整数会被提升（`opacity = 1` 应当等于 `1.0`）。
    #[must_use]
    pub fn as_float(&self) -> Option<f64> {
        match self {
            Self::Float(v) => Some(*v),
            Self::Int(v) => Some(*v as f64),
            _ => None,
        }
    }

    /// 取字符串。
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(s) => Some(s),
            _ => None,
        }
    }

    /// 取字符串数组；含非字符串元素时返回 `None`（整条作废，不做"跳过坏元素"）。
    #[must_use]
    pub fn as_str_array(&self) -> Option<Vec<String>> {
        let Self::Array(items) = self else {
            return None;
        };
        items
            .iter()
            .map(|v| v.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()
    }

    /// 类型名，用于告警文案。
    #[must_use]
    pub const fn type_name(&self) -> &'static str {
        match self {
            Self::Bool(_) => "布尔",
            Self::Int(_) => "整数",
            Self::Float(_) => "浮点",
            Self::Str(_) => "字符串",
            Self::Array(_) => "数组",
        }
    }
}

/// 一条键值及其位置。
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    /// 所属 section；`None` 表示根级（如 `version`）。
    pub section: Option<String>,
    /// 键名。
    pub key: String,
    /// 值。
    pub value: Value,
    /// 在源文件中的行号（从 1 开始），告警里要带上，否则用户不知道改哪一行。
    pub line: usize,
}

/// 解析结果。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Document {
    entries: Vec<Entry>,
    sections: Vec<String>,
}

impl Document {
    /// 取某个键。
    #[must_use]
    pub fn get(&self, section: Option<&str>, key: &str) -> Option<&Value> {
        self.entries
            .iter()
            .find(|e| e.section.as_deref() == section && e.key == key)
            .map(|e| &e.value)
    }

    /// 取某个键连同行号（告警要带行号）。
    #[must_use]
    pub fn get_entry(&self, section: Option<&str>, key: &str) -> Option<&Entry> {
        self.entries
            .iter()
            .find(|e| e.section.as_deref() == section && e.key == key)
    }

    /// 全部条目（按出现顺序）。
    #[must_use]
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// 出现过的 section 名（按出现顺序，可能重复？不会：重复的会合并，只记一次）。
    #[must_use]
    pub fn section_names(&self) -> &[String] {
        &self.sections
    }

    /// 是否为空文档（等价于全默认配置）。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// 解析错误（带行号）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TomlError {
    /// 出错行号（从 1 开始）。
    pub line: usize,
    /// 人类可读的原因。
    pub message: String,
}

impl fmt::Display for TomlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "第 {} 行：{}", self.line, self.message)
    }
}

impl std::error::Error for TomlError {}

/// 解析 TOML 子集。
///
/// # Errors
///
/// 语法错误、重复键、不支持的语法结构。
pub fn parse(src: &str) -> Result<Document, TomlError> {
    Parser::new(src).run()
}

/// 浮点写成 TOML 时**必须**带小数点：`1` 是整数，读回来会撞类型告警。
#[must_use]
pub fn format_float(v: f64) -> String {
    if !v.is_finite() {
        return "0.0".to_owned();
    }
    let s = format!("{v}");
    if s.contains('.') || s.contains('e') || s.contains('E') {
        s
    } else {
        format!("{s}.0")
    }
}

/// 把一个值写成 TOML 文本（用于回写"本版本不认识的键"）。
#[must_use]
pub fn value_to_string(value: &Value) -> String {
    match value {
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => format_float(*f),
        Value::Str(s) => quote(s),
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(value_to_string).collect();
            format!("[{}]", parts.join(", "))
        }
    }
}

/// 把字符串写成 TOML 基本字符串（含转义与引号）。
#[must_use]
pub fn quote(s: &str) -> String {
    use std::fmt::Write as _;

    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if u32::from(c) < 0x20 => {
                let _ = write!(out, "\\u{:04X}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn is_bare(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'-'
}

/// 数组/内联结构最大嵌套层数。
///
/// 解析器是递归下降；不设上限时，恶意构造的 `[[[[…]]]]` 会耗尽栈。
const MAX_NESTING_DEPTH: usize = 32;

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
    line: usize,
}

impl<'a> Parser<'a> {
    fn new(src: &'a str) -> Self {
        let bytes = src.as_bytes();
        // UTF-8 BOM：Windows 编辑器会给，直接吃掉，别让它变成"非法字符"。
        let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
        Self {
            bytes,
            pos: 0,
            line: 1,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let b = self.peek()?;
        self.pos += 1;
        if b == b'\n' {
            self.line += 1;
        }
        Some(b)
    }

    fn fail<T>(&self, message: impl Into<String>) -> Result<T, TomlError> {
        Err(TomlError {
            line: self.line,
            message: message.into(),
        })
    }

    fn skip_inline_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\r')) {
            self.bump();
        }
    }

    fn skip_to_eol(&mut self) {
        while !matches!(self.peek(), None | Some(b'\n')) {
            self.bump();
        }
    }

    /// 跳过空白、换行与注释（顶层与数组内部都用）。
    fn skip_trivia(&mut self) {
        loop {
            match self.peek() {
                Some(b' ' | b'\t' | b'\r' | b'\n') => {
                    self.bump();
                }
                Some(b'#') => {
                    self.skip_to_eol();
                }
                _ => return,
            }
        }
    }

    fn end_of_line(&mut self) -> Result<(), TomlError> {
        self.skip_inline_ws();
        if self.peek() == Some(b'#') {
            self.skip_to_eol();
        }
        match self.peek() {
            None => Ok(()),
            Some(b'\n') => {
                self.bump();
                Ok(())
            }
            Some(b'\r') => {
                self.bump();
                if self.peek() == Some(b'\n') {
                    self.bump();
                }
                Ok(())
            }
            Some(c) => self.fail(format!("值后面有多余字符 {:?}", char::from(c))),
        }
    }

    fn run(mut self) -> Result<Document, TomlError> {
        let mut doc = Document::default();
        let mut section: Option<String> = None;
        // 重复键检查用线性扫描：本项目最多 40 个键，O(n²) 完全够用且无额外依赖。
        let mut seen: Vec<(Option<String>, String)> = Vec::new();

        loop {
            self.skip_trivia();
            let Some(c) = self.peek() else { break };
            if c == b'[' {
                let name = self.section_header()?;
                if !doc.sections.iter().any(|s| s == &name) {
                    doc.sections.push(name.clone());
                }
                section = Some(name);
                continue;
            }

            let line = self.line;
            let key = self.key()?;
            self.skip_inline_ws();
            if self.peek() != Some(b'=') {
                return self.fail(format!("键 {key} 后面缺少 `=`"));
            }
            self.bump();
            self.skip_inline_ws();
            let value = self.value(0)?;
            self.end_of_line()?;

            if seen
                .iter()
                .any(|(s, k)| s.as_deref() == section.as_deref() && k == &key)
            {
                return self.fail(format!("重复的键 {key}（同一 section 内只能出现一次）"));
            }
            seen.push((section.clone(), key.clone()));
            doc.entries.push(Entry {
                section: section.clone(),
                key,
                value,
                line,
            });
        }
        Ok(doc)
    }

    fn section_header(&mut self) -> Result<String, TomlError> {
        self.bump(); // '['
        if self.peek() == Some(b'[') {
            return self.fail("不支持数组表 `[[...]]`");
        }
        let mut buf: Vec<u8> = Vec::new();
        loop {
            match self.peek() {
                None => return self.fail("section 头未闭合"),
                Some(b'\n') => return self.fail("section 头未闭合（缺少 `]`）"),
                Some(b']') => {
                    self.bump();
                    break;
                }
                Some(b'"') => {
                    let s = self.basic_string()?;
                    buf.extend_from_slice(s.as_bytes());
                }
                Some(b'\'') => {
                    let s = self.literal_string()?;
                    buf.extend_from_slice(s.as_bytes());
                }
                Some(b'.') => {
                    return self.fail("不支持点号分层 section，请用单层 `[visual]` 这种写法");
                }
                Some(c) if is_bare(c) => {
                    buf.push(c);
                    self.bump();
                }
                Some(b' ' | b'\t') => {
                    self.bump();
                }
                Some(c) => return self.fail(format!("section 名含非法字符 {:?}", char::from(c))),
            }
        }
        self.end_of_line()?;
        if buf.is_empty() {
            return self.fail("section 名为空");
        }
        String::from_utf8(buf).map_err(|_| TomlError {
            line: self.line,
            message: "section 名不是合法 UTF-8".to_owned(),
        })
    }

    fn key(&mut self) -> Result<String, TomlError> {
        match self.peek() {
            Some(b'"') => self.basic_string(),
            Some(b'\'') => self.literal_string(),
            _ => {
                let mut buf: Vec<u8> = Vec::new();
                loop {
                    match self.peek() {
                        Some(c) if is_bare(c) => {
                            buf.push(c);
                            self.bump();
                        }
                        Some(b'.') => {
                            return self
                                .fail("不支持点号嵌套键 `a.b`，请用 `[section]` + `b` 写法");
                        }
                        _ => break,
                    }
                }
                if buf.is_empty() {
                    return self.fail("缺少键名");
                }
                // 裸键只可能是 ASCII，这里不会失败；仍保留错误路径以免将来放宽字符集时 panic。
                String::from_utf8(buf).map_err(|_| TomlError {
                    line: self.line,
                    message: "键名不是合法 UTF-8".to_owned(),
                })
            }
        }
    }

    fn value(&mut self, depth: usize) -> Result<Value, TomlError> {
        if depth > MAX_NESTING_DEPTH {
            return self.fail(format!("嵌套超过上限（{MAX_NESTING_DEPTH} 层）"));
        }
        match self.peek() {
            None => self.fail("缺少值"),
            Some(b'"') => Ok(Value::Str(self.basic_string()?)),
            Some(b'\'') => Ok(Value::Str(self.literal_string()?)),
            Some(b'[') => self.array(depth),
            Some(_) => self.scalar(),
        }
    }

    fn scalar(&mut self) -> Result<Value, TomlError> {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if matches!(c, b' ' | b'\t' | b'\r' | b'\n' | b'#' | b',' | b']') {
                break;
            }
            self.bump();
        }
        let token = std::str::from_utf8(&self.bytes[start..self.pos]).unwrap_or("");
        if token.is_empty() {
            return self.fail("缺少值");
        }
        match token {
            "true" => return Ok(Value::Bool(true)),
            "false" => return Ok(Value::Bool(false)),
            "inf" | "+inf" => return Ok(Value::Float(f64::INFINITY)),
            "-inf" => return Ok(Value::Float(f64::NEG_INFINITY)),
            "nan" | "+nan" | "-nan" => return Ok(Value::Float(f64::NAN)),
            _ => {}
        }
        if token.starts_with("0x") || token.starts_with("0b") || token.starts_with("0o") {
            return self.fail(format!("不支持 {token} 这种进制前缀，请写十进制"));
        }
        let cleaned: String = token.chars().filter(|&c| c != '_').collect();
        let is_float = cleaned.contains('.')
            || cleaned.contains('e')
            || cleaned.contains('E')
            || cleaned.contains("inf")
            || cleaned.contains("nan");
        if is_float {
            cleaned
                .parse::<f64>()
                .map(Value::Float)
                .map_err(|_| TomlError {
                    line: self.line,
                    message: format!("非法浮点数 {token}"),
                })
        } else {
            cleaned
                .parse::<i64>()
                .map(Value::Int)
                .map_err(|_| TomlError {
                    line: self.line,
                    message: format!("非法整数 {token}"),
                })
        }
    }

    fn array(&mut self, depth: usize) -> Result<Value, TomlError> {
        self.bump(); // '['
        let mut items = Vec::new();
        loop {
            self.skip_trivia();
            match self.peek() {
                None => return self.fail("数组未闭合（缺少 `]`）"),
                Some(b']') => {
                    self.bump();
                    break;
                }
                _ => {}
            }
            items.push(self.value(depth + 1)?);
            self.skip_trivia();
            match self.peek() {
                Some(b',') => {
                    self.bump();
                }
                Some(b']') => {
                    self.bump();
                    break;
                }
                None => return self.fail("数组未闭合（缺少 `]`）"),
                Some(c) => {
                    return self.fail(format!("数组元素之间缺少逗号（遇到 {:?}）", char::from(c)));
                }
            }
        }
        Ok(Value::Array(items))
    }

    fn basic_string(&mut self) -> Result<String, TomlError> {
        self.bump(); // '"'
        let mut buf: Vec<u8> = Vec::new();
        loop {
            match self.bump() {
                None => return self.fail("字符串未闭合"),
                Some(b'"') => break,
                Some(b'\n') => return self.fail("字符串不能跨行（多行字符串未支持）"),
                Some(b'\\') => buf.extend_from_slice(self.escape()?.as_bytes()),
                Some(c) => buf.push(c),
            }
        }
        String::from_utf8(buf).map_err(|_| TomlError {
            line: self.line,
            message: "字符串不是合法 UTF-8".to_owned(),
        })
    }

    fn escape(&mut self) -> Result<String, TomlError> {
        match self.bump() {
            Some(b'"') => Ok("\"".to_owned()),
            Some(b'\\') => Ok("\\".to_owned()),
            Some(b'n') => Ok("\n".to_owned()),
            Some(b'r') => Ok("\r".to_owned()),
            Some(b't') => Ok("\t".to_owned()),
            Some(b'b') => Ok("\u{8}".to_owned()),
            Some(b'f') => Ok("\u{c}".to_owned()),
            Some(b'u') => self.unicode_escape(4),
            Some(b'U') => self.unicode_escape(8),
            Some(c) => self.fail(format!("不支持的转义 `\\{}`", char::from(c))),
            None => self.fail("字符串在转义符后结束"),
        }
    }

    fn unicode_escape(&mut self, digits: u32) -> Result<String, TomlError> {
        let mut code = 0_u32;
        for _ in 0..digits {
            let Some(c) = self.bump() else {
                return self.fail("`\\u` 转义不完整");
            };
            let Some(v) = (char::from(c)).to_digit(16) else {
                return self.fail(format!("`\\u` 转义含非十六进制字符 {:?}", char::from(c)));
            };
            code = code * 16 + v;
        }
        char::from_u32(code)
            .map(|c| c.to_string())
            .ok_or_else(|| TomlError {
                line: self.line,
                message: format!("`\\u` 转义 {code:#X} 不是合法字符"),
            })
    }

    fn literal_string(&mut self) -> Result<String, TomlError> {
        self.bump(); // '\''
        let mut buf: Vec<u8> = Vec::new();
        loop {
            match self.bump() {
                None => return self.fail("字面字符串未闭合"),
                Some(b'\'') => break,
                Some(b'\n') => return self.fail("字符串不能跨行（多行字符串未支持）"),
                Some(c) => buf.push(c),
            }
        }
        String::from_utf8(buf).map_err(|_| TomlError {
            line: self.line,
            message: "字符串不是合法 UTF-8".to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{Document, TomlError, Value, format_float, parse, quote, value_to_string};

    fn doc(src: &str) -> Document {
        parse(src).expect("解析失败")
    }

    #[test]
    fn deeply_nested_arrays_are_rejected() {
        // 审计 AUD-036：超深嵌套不能让解析器栈溢出。
        let text = format!("a = {}0{}\n", "[".repeat(500), "]".repeat(500));
        let err = parse(&text).expect_err("超深嵌套应返回解析错误");
        assert!(
            err.message.contains("嵌套"),
            "错误信息应说明嵌套上限：{err:?}"
        );
    }

    #[test]
    fn empty_document() {
        let d = doc("   \n# 只有注释\n\n");
        assert!(d.is_empty());
        assert!(d.section_names().is_empty());
    }

    #[test]
    fn scalars_and_sections() {
        let d = doc(r##"
version = 1

[general]
enabled = true
fps = -30

[visual]
opacity = 0.65
bands = 64
color_start = "#00C8FF"
name = 'literal\no'
"##);
        assert_eq!(d.get(None, "version"), Some(&Value::Int(1)));
        assert_eq!(d.get(Some("general"), "enabled"), Some(&Value::Bool(true)));
        assert_eq!(d.get(Some("general"), "fps"), Some(&Value::Int(-30)));
        assert_eq!(d.get(Some("visual"), "opacity"), Some(&Value::Float(0.65)));
        assert_eq!(
            d.get(Some("visual"), "color_start"),
            Some(&Value::Str("#00C8FF".to_owned()))
        );
        // 字面字符串不处理转义
        assert_eq!(
            d.get(Some("visual"), "name"),
            Some(&Value::Str("literal\\no".to_owned()))
        );
        assert_eq!(
            d.section_names().to_vec(),
            vec!["general".to_owned(), "visual".to_owned()]
        );
    }

    #[test]
    fn comment_after_value_and_hash_inside_string() {
        let d = doc("a = \"#not-a-comment\" # 这才是注释\n");
        assert_eq!(d.get(None, "a").unwrap().as_str(), Some("#not-a-comment"));
    }

    #[test]
    fn escapes() {
        let d = doc(r#"a = "x\ty\n\"q\"\\\u4E2D""#);
        assert_eq!(d.get(None, "a").unwrap().as_str(), Some("x\ty\n\"q\"\\中"));
    }

    #[test]
    fn arrays_multiline_with_trailing_comma() {
        let d = doc(r#"
[outputs]
names = [
  "DP-1",   # 注释也能出现在数组里
  "HDMI-1",
]
empty = []
"#);
        assert_eq!(
            d.get(Some("outputs"), "names").unwrap().as_str_array(),
            Some(vec!["DP-1".to_owned(), "HDMI-1".to_owned()])
        );
        assert_eq!(
            d.get(Some("outputs"), "empty").unwrap().as_str_array(),
            Some(Vec::new())
        );
    }

    #[test]
    fn numbers() {
        let d = doc("a = 1_000\nb = 1e3\nc = +2.5\nd = 1\n");
        assert_eq!(d.get(None, "a"), Some(&Value::Int(1000)));
        assert_eq!(d.get(None, "b"), Some(&Value::Float(1000.0)));
        assert_eq!(d.get(None, "c"), Some(&Value::Float(2.5)));
        // 整数可以被当浮点读，但反过来不行
        assert_eq!(d.get(None, "d").unwrap().as_float(), Some(1.0));
        assert_eq!(Value::Float(1.5).as_int(), None, "浮点不得隐式取整");
    }

    #[test]
    fn bom_and_crlf() {
        let d = doc("\u{feff}version = 1\r\n[visual]\r\nopacity = 0.5\r\n");
        assert_eq!(d.get(None, "version"), Some(&Value::Int(1)));
        assert_eq!(d.get(Some("visual"), "opacity"), Some(&Value::Float(0.5)));
    }

    #[test]
    fn duplicate_key_is_error() {
        let err = parse("[visual]\nopacity = 0.5\nopacity = 0.6\n").expect_err("应当报错");
        assert!(err.message.contains("重复的键"), "{err}");
    }

    #[test]
    fn repeated_section_is_merged() {
        let d = doc("[visual]\nopacity = 0.5\n[visual]\nbands = 32\n");
        assert_eq!(d.get(Some("visual"), "opacity"), Some(&Value::Float(0.5)));
        assert_eq!(d.get(Some("visual"), "bands"), Some(&Value::Int(32)));
    }

    #[test]
    fn line_numbers_are_reported() {
        let err = parse("[visual]\nopacity = 0.5\nbad line\n").expect_err("应当报错");
        assert_eq!(err.line, 3, "{err}");
    }

    #[test]
    fn unsupported_syntax_is_rejected() {
        for src in [
            "[[table]]\na = 1\n",
            "a.b = 1\n",
            "[a.b]\n",
            "d = 2026-11-14\n",
            "s = \"\"\"x\"\"\"\n",
            "h = 0xFF\n",
        ] {
            assert!(parse(src).is_err(), "应当拒绝不支持的语法：{src:?}");
        }
    }

    #[test]
    fn unterminated_constructs() {
        for src in ["a = \"x\n", "a = [1, 2\n", "[visual\n", "a = 1 b = 2\n"] {
            assert!(parse(src).is_err(), "应当报错：{src:?}");
        }
    }

    #[test]
    fn error_display_has_line() {
        let err = TomlError {
            line: 7,
            message: "x".to_owned(),
        };
        assert_eq!(err.to_string(), "第 7 行：x");
    }

    #[test]
    fn quote_round_trips() {
        for s in ["", "abc", "带有中文", "a\"b", "a\\b", "a\tb", "#x"] {
            let src = format!("k = {}\n", quote(s));
            assert_eq!(
                doc(&src).get(None, "k").unwrap().as_str(),
                Some(s),
                "往返失败：{s:?}"
            );
        }
    }

    #[test]
    fn truncation_never_panics() {
        // 编辑器把文件写一半是热重载最常见的输入；任何前缀都只能"报错"，不能 panic。
        let full = "[general]\nenabled = true\n[visual]\nopacity = 0.65\nnames = [\"a\", \"b\"]\n";
        for cut in 0..=full.len() {
            if !full.is_char_boundary(cut) {
                continue;
            }
            let _ = parse(&full[..cut]);
        }
    }

    #[test]
    fn value_to_string_round_trips() {
        assert_eq!(value_to_string(&Value::Bool(true)), "true");
        assert_eq!(value_to_string(&Value::Int(-3)), "-3");
        assert_eq!(value_to_string(&Value::Float(1.0)), "1.0");
        assert_eq!(value_to_string(&Value::Float(0.5)), "0.5");
        assert_eq!(value_to_string(&Value::Str("a#b".into())), "\"a#b\"");
        assert_eq!(
            value_to_string(&Value::Array(vec![Value::Str("x".into()), Value::Int(1)])),
            "[\"x\", 1]"
        );
    }

    #[test]
    fn format_float_keeps_decimal_point() {
        assert_eq!(format_float(1.0), "1.0");
        assert_eq!(format_float(0.65), "0.65");
        assert_eq!(format_float(f64::NAN), "0.0");
    }

    #[test]
    fn control_char_in_string_is_quoted() {
        let s = quote("a\u{1}b");
        assert_eq!(s, "\"a\\u0001b\"");
        assert_eq!(
            doc(&format!("k = {s}\n")).get(None, "k").unwrap().as_str(),
            Some("a\u{1}b")
        );
    }
}
