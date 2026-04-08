use std::path::Path;

pub fn join_text(parts: &[&str]) -> String {
    let capacity = parts.iter().map(|part| part.len()).sum();
    let mut output = String::with_capacity(capacity);
    for part in parts {
        output.push_str(part);
    }
    output
}

pub fn prefixed_text(prefix: &str, suffix: &str) -> String {
    join_text(&[prefix, suffix])
}

pub fn prefixed_display(prefix: &str, value: &impl std::fmt::Display) -> String {
    let value_text = value.to_string();
    prefixed_text(prefix, value_text.as_str())
}

pub fn quoted_text(prefix: &str, name: &str, suffix: &str) -> String {
    join_text(&[prefix, "'", name, "'", suffix])
}

pub fn path_message(prefix: &str, path: &Path) -> String {
    let path_text = path.display().to_string();
    join_text(&[prefix, path_text.as_str()])
}

pub fn path_error_text(prefix: &str, path: &Path, error: &impl std::fmt::Display) -> String {
    let path_text = path.display().to_string();
    let error_text = error.to_string();
    join_text(&[prefix, path_text.as_str(), ": ", error_text.as_str()])
}
