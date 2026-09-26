//! Minimal JSON model and serializer.
//!
//! The `--json` output mode needs a tiny, dependency-free JSON writer. Rather
//! than formatting strings by hand all over the output layer, results are
//! first built into a [`Json`] tree and then serialized in one place. The
//! writer always produces compact, single-line JSON suitable for streaming
//! (one object per line, JSON-Lines style).

use std::fmt::Write as FmtWrite;

/// A JSON value. Object keys keep insertion order so output is deterministic.
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    /// The JSON `null` literal.
    Null,
    /// `true` / `false`.
    Bool(bool),
    /// A signed integer (line counts can never be negative, but the variant
    /// keeps the model generic).
    Int(i64),
    /// An unsigned integer — used for line numbers, byte counts, etc.
    Uint(u64),
    /// A finite floating point number; non-finite values serialize as `null`.
    Float(f64),
    /// A UTF-8 string, escaped on write.
    Str(String),
    /// An ordered array.
    Array(Vec<Json>),
    /// An ordered mapping of keys to values.
    Object(Vec<(String, Json)>),
}

impl Json {
    /// Shorthand constructor for [`Json::Str`].
    pub fn string(value: impl Into<String>) -> Json {
        Json::Str(value.into())
    }

    /// Shorthand constructor for [`Json::Object`] that borrows the keys.
    pub fn object(pairs: Vec<(&str, Json)>) -> Json {
        Json::Object(
            pairs
                .into_iter()
                .map(|(key, value)| (key.to_string(), value))
                .collect(),
        )
    }

    /// Serialize the value into a compact single-line JSON string.
    pub fn to_json_string(&self) -> String {
        let mut out = String::new();
        self.write_into(&mut out);
        out
    }

    /// Recursive serializer writing into an existing [`String`] buffer.
    fn write_into(&self, out: &mut String) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(true) => out.push_str("true"),
            Json::Bool(false) => out.push_str("false"),
            Json::Int(n) => {
                let _ = write!(out, "{}", n);
            }
            Json::Uint(n) => {
                let _ = write!(out, "{}", n);
            }
            Json::Float(f) => {
                if f.is_finite() {
                    let _ = write!(out, "{}", f);
                } else {
                    out.push_str("null");
                }
            }
            Json::Str(s) => {
                out.push('"');
                out.push_str(&escape_json(s));
                out.push('"');
            }
            Json::Array(items) => {
                out.push('[');
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    item.write_into(out);
                }
                out.push(']');
            }
            Json::Object(pairs) => {
                out.push('{');
                for (index, (key, value)) in pairs.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    out.push('"');
                    out.push_str(&escape_json(key));
                    out.push_str("\":");
                    value.write_into(out);
                }
                out.push('}');
            }
        }
    }
}

/// Escape a string for embedding inside a JSON string literal.
///
/// Handles the two mandatory escapes (`"` and `\`), the five short control
/// escapes (`\b`, `\f`, `\n`, `\r`, `\t`) and emits every other control
/// character below `0x20` as `\u00XX`. All other characters (including any
/// valid UTF-8) are passed through unchanged.
pub fn escape_json(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 2);
    for ch in input.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0C}' => out.push_str("\\f"),
            other if (other as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", other as u32);
            }
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalars_serialize_correctly() {
        assert_eq!(Json::Null.to_json_string(), "null");
        assert_eq!(Json::Bool(true).to_json_string(), "true");
        assert_eq!(Json::Bool(false).to_json_string(), "false");
        assert_eq!(Json::Int(-12).to_json_string(), "-12");
        assert_eq!(Json::Uint(42).to_json_string(), "42");
        assert_eq!(Json::Float(1.5).to_json_string(), "1.5");
        assert_eq!(Json::Float(f64::NAN).to_json_string(), "null");
        assert_eq!(Json::Float(f64::INFINITY).to_json_string(), "null");
    }

    #[test]
    fn strings_are_escaped() {
        assert_eq!(Json::string("quote\" back\\slash\n").to_json_string(), "\"quote\\\" back\\\\slash\\n\"");
        assert_eq!(Json::string("tab\there").to_json_string(), "\"tab\\there\"");
        assert_eq!(escape_json("\u{1}"), "\\u0001");
        assert_eq!(escape_json("keep ✓ ünïcode"), "keep ✓ ünïcode");
    }

    #[test]
    fn arrays_and_objects_nest() {
        let value = Json::object(vec![
            ("path", Json::string("src/main.rs")),
            ("binary", Json::Bool(false)),
            ("ranges", Json::Array(vec![
                Json::Array(vec![Json::Uint(0), Json::Uint(3)]),
                Json::Array(vec![Json::Uint(8), Json::Uint(11)]),
            ])),
            ("nested", Json::object(vec![("empty", Json::Array(vec![]))])),
        ]);
        let expected = concat!(
            "{\"path\":\"src/main.rs\",\"binary\":false,",
            "\"ranges\":[[0,3],[8,11]],",
            "\"nested\":{\"empty\":[]}}"
        );
        assert_eq!(value.to_json_string(), expected);
    }

    #[test]
    fn object_preserves_key_order() {
        let value = Json::object(vec![
            ("z", Json::Uint(1)),
            ("a", Json::Uint(2)),
            ("m", Json::Uint(3)),
        ]);
        assert_eq!(value.to_json_string(), "{\"z\":1,\"a\":2,\"m\":3}");
    }

    #[test]
    fn empty_containers() {
        assert_eq!(Json::Array(vec![]).to_json_string(), "[]");
        assert_eq!(Json::Object(vec![]).to_json_string(), "{}");
    }
}
