//! String helpers that count characters, not bytes: titles and comments are
//! arbitrary Unicode, and a byte slice through one would panic.

use serde_json::Value;

/// At most the first `n` characters of `text`.
pub fn prefix(text: &str, n: usize) -> &str {
    match text.char_indices().nth(n) {
        Some((end, _)) => &text[..end],
        None => text,
    }
}

/// Whitespace collapsed to single spaces, cut to `n` characters with an ellipsis.
pub fn clip(text: &str, n: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= n {
        flat
    } else {
        format!("{}…", prefix(&flat, n - 1))
    }
}

/// The last `/`-separated part of a path.
pub fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

pub fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// Whether a JSON value carries anything: not null, false, zero or empty. What the
/// model and GitHub send is judged by this rather than by its type.
pub fn is_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64() != Some(0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(list) => !list.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}
