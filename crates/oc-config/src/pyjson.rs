//! JSON written byte-for-byte like Python's `json.dumps(..., indent=2)`, so a
//! file saved by either version diffs cleanly against the other — and small
//! helpers that read loosely typed JSON the way the Python code did.

use std::io;

use serde::Serialize;
use serde_json::Value;
use serde_json::ser::{CharEscape, Formatter, PrettyFormatter};

/// `json.dumps(value, indent=2, ensure_ascii=...)`.
///
/// Python's default (`ensure_ascii=True`) escapes everything outside
/// printable ASCII as `\uXXXX` (DEL included, surrogate pairs above the BMP);
/// serde_json writes UTF-8. Indentation and separators already match.
pub(crate) fn dumps<T: Serialize + ?Sized>(value: &T, ensure_ascii: bool) -> String {
    let mut out = Vec::with_capacity(512);
    let formatter = PyFormatter {
        pretty: PrettyFormatter::with_indent(b"  "),
        ensure_ascii,
    };
    let mut ser = serde_json::Serializer::with_formatter(&mut out, formatter);
    // Serializing plain data into a Vec can't fail.
    value
        .serialize(&mut ser)
        .expect("JSON serialization into memory");
    // The formatter only ever writes valid UTF-8 (ASCII escapes or the input's UTF-8).
    String::from_utf8(out).expect("JSON output is UTF-8")
}

struct PyFormatter {
    pretty: PrettyFormatter<'static>,
    ensure_ascii: bool,
}

impl Formatter for PyFormatter {
    fn begin_array<W: ?Sized + io::Write>(&mut self, w: &mut W) -> io::Result<()> {
        self.pretty.begin_array(w)
    }
    fn end_array<W: ?Sized + io::Write>(&mut self, w: &mut W) -> io::Result<()> {
        self.pretty.end_array(w)
    }
    fn begin_array_value<W: ?Sized + io::Write>(
        &mut self,
        w: &mut W,
        first: bool,
    ) -> io::Result<()> {
        self.pretty.begin_array_value(w, first)
    }
    fn end_array_value<W: ?Sized + io::Write>(&mut self, w: &mut W) -> io::Result<()> {
        self.pretty.end_array_value(w)
    }
    fn begin_object<W: ?Sized + io::Write>(&mut self, w: &mut W) -> io::Result<()> {
        self.pretty.begin_object(w)
    }
    fn end_object<W: ?Sized + io::Write>(&mut self, w: &mut W) -> io::Result<()> {
        self.pretty.end_object(w)
    }
    fn begin_object_key<W: ?Sized + io::Write>(
        &mut self,
        w: &mut W,
        first: bool,
    ) -> io::Result<()> {
        self.pretty.begin_object_key(w, first)
    }
    fn begin_object_value<W: ?Sized + io::Write>(&mut self, w: &mut W) -> io::Result<()> {
        self.pretty.begin_object_value(w)
    }
    fn end_object_value<W: ?Sized + io::Write>(&mut self, w: &mut W) -> io::Result<()> {
        self.pretty.end_object_value(w)
    }

    fn write_string_fragment<W: ?Sized + io::Write>(
        &mut self,
        w: &mut W,
        fragment: &str,
    ) -> io::Result<()> {
        if !self.ensure_ascii || fragment.bytes().all(|b| b < 0x7f) {
            return w.write_all(fragment.as_bytes());
        }
        let mut start = 0;
        for (i, c) in fragment.char_indices() {
            if (c as u32) < 0x7f {
                continue;
            }
            w.write_all(&fragment.as_bytes()[start..i])?;
            let mut units = [0u16; 2];
            for unit in c.encode_utf16(&mut units) {
                write!(w, "\\u{unit:04x}")?;
            }
            start = i + c.len_utf8();
        }
        w.write_all(&fragment.as_bytes()[start..])
    }

    /// Python's `repr(float)`: shortest round-trip digits like serde_json, but
    /// positional for exponents -4..16 ("1e+16", "1e-05", "100.0"), which
    /// ryu formats differently. serde_json writes non-finite values as null
    /// before getting here.
    fn write_f64<W: ?Sized + io::Write>(&mut self, w: &mut W, value: f64) -> io::Result<()> {
        let sci = format!("{value:e}"); // e.g. "1.5e-7", "-1e16", "0e0"
        let (mantissa, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
        let exp: i32 = exp.parse().unwrap_or(0);
        if (-4..16).contains(&exp) {
            let fixed = value.to_string(); // shortest digits, never an exponent
            w.write_all(fixed.as_bytes())?;
            if !fixed.contains('.') {
                w.write_all(b".0")?;
            }
            Ok(())
        } else {
            let sign = if exp < 0 { '-' } else { '+' };
            write!(w, "{mantissa}e{sign}{:02}", exp.unsigned_abs())
        }
    }

    fn write_char_escape<W: ?Sized + io::Write>(
        &mut self,
        w: &mut W,
        escape: CharEscape,
    ) -> io::Result<()> {
        self.pretty.write_char_escape(w, escape)
    }
}

// ---------------------------------------------------------------------------
// Loose reading
// ---------------------------------------------------------------------------

/// Python's `str(value)` for JSON values, for fields the Python code coerced
/// with `str(...)` instead of validating.
pub(crate) fn py_str(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

/// Python truthiness of a JSON value (`x or default`).
pub(crate) fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn matches_python_layout() {
        let v =
            json!({"a": 1, "b": [1, 2], "c": {}, "d": [], "e": null, "f": 1.0, "g": {"x": true}});
        // json.dumps(v, indent=2)
        let expected = "{\n  \"a\": 1,\n  \"b\": [\n    1,\n    2\n  ],\n  \"c\": {},\n  \"d\": [],\n  \"e\": null,\n  \"f\": 1.0,\n  \"g\": {\n    \"x\": true\n  }\n}";
        assert_eq!(dumps(&v, true), expected);
    }

    #[test]
    fn ascii_escapes_like_python() {
        // json.dumps("é\x7f😀\n\"", ensure_ascii=True)
        assert_eq!(
            dumps(&json!("\u{e9}\u{7f}\u{1F600}\n\""), true),
            "\"\\u00e9\\u007f\\ud83d\\ude00\\n\\\"\""
        );
        assert_eq!(dumps(&json!("é\u{7f}\u{1}"), false), "\"é\u{7f}\\u0001\"");
    }

    #[test]
    fn floats_like_python_repr() {
        // json.dumps([...]) in CPython.
        let v = json!([
            1.0,
            -0.0,
            1.25,
            0.1,
            1e16,
            1e15,
            1e-5,
            1e-4,
            1.5e-7,
            1e100,
            123456789012345.67,
            9999999999999998.0,
            -2.5e-300,
            5e-324,
            1.7976931348623157e308
        ]);
        let expected = "[1.0, -0.0, 1.25, 0.1, 1e+16, 1000000000000000.0, 1e-05, 0.0001, 1.5e-07, \
                        1e+100, 123456789012345.67, 9999999999999998.0, -2.5e-300, 5e-324, \
                        1.7976931348623157e+308]";
        let out = dumps(&v, true).replace("\n  ", " ").replace('\n', "");
        assert_eq!(out.replace("[ ", "["), expected);
    }

    #[test]
    fn loose_values() {
        assert_eq!(py_str(&json!(5)), "5");
        assert_eq!(py_str(&json!(true)), "True");
        assert!(!truthy(&json!("")) && !truthy(&json!(0)) && truthy(&json!("x")));
    }
}
