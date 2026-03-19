pub(crate) fn split_shell_words(input: &str, label: &str) -> anyhow::Result<Vec<String>> {
    shell_words::split(input).map_err(|error| {
        let mut message = String::from(label);
        message.push_str(" command contains invalid shell quoting: ");
        message.push_str(error.to_string().as_str());
        anyhow::anyhow!(message)
    })
}
