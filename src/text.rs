//! String helpers that count characters, not bytes: titles and comments are
//! arbitrary Unicode, and a byte slice through one would panic.

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
