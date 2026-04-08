#[derive(Debug, thiserror::Error)]
#[error("{label} command contains invalid shell quoting: {source}")]
pub struct SplitShellWordsError {
    label: String,
    #[source]
    source: shell_words::ParseError,
}

pub fn split_shell_words(input: &str, label: &str) -> Result<Vec<String>, SplitShellWordsError> {
    shell_words::split(input).map_err(|source| SplitShellWordsError {
        label: label.to_string(),
        source,
    })
}
