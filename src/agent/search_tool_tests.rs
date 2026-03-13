#[cfg(test)]
mod tests {
    use super::super::search_tool::*;

    fn base_args() -> SearchArgs {
        SearchArgs {
            command: None,
            action: "query".to_string(),
            path: None,
            query: Some("hello".to_string()),
            search_mode: "hybrid".to_string(),
            output_mode: "files".to_string(),
            limit: 20,
            offset: 0,
            scan_limit: 2000,
            case_sensitive: false,
            regex: None,
            fuzzy_distance: 1,
            include_content: false,
        }
    }

    #[test]
    fn parses_index_command_form() {
        let mut args = base_args();
        apply_command_form(&mut args, "search index ./docs").expect("index command should parse");
        assert_eq!(args.action, "index");
        assert_eq!(args.path.as_deref(), Some("./docs"));
    }

    #[test]
    fn parses_query_command_form() {
        let mut args = base_args();
        apply_command_form(&mut args, "search query concurrency bug").expect("query command should parse");
        assert_eq!(args.action, "query");
        assert_eq!(args.query.as_deref(), Some("concurrency bug"));
    }

    #[test]
    fn chunks_long_text() {
        let content = "a".repeat(6000);
        let chunks = chunk_text(content.as_str());
        assert!(chunks.len() >= 2);
        assert!(chunks.iter().all(|chunk| !chunk.is_empty()));
    }
}
