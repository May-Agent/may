#[cfg(test)]
mod tests {
    use super::super::search_tool::*;

    fn base_args() -> SearchArgs {
        SearchArgs {
            command: None,
            action: SearchAction::Query,
            path: None,
            query: Some("hello".to_string()),
            search_mode: SearchMode::Hybrid,
            output_mode: SearchOutputMode::Files,
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
        assert_eq!(args.action, SearchAction::Index);
        assert_eq!(args.path.as_deref(), Some("./docs"));
    }

    #[test]
    fn parses_query_command_form() {
        let mut args = base_args();
        apply_command_form(&mut args, "search query concurrency bug")
            .expect("query command should parse");
        assert_eq!(args.action, SearchAction::Query);
        assert_eq!(args.query.as_deref(), Some("concurrency bug"));
    }

    #[test]
    fn parses_quoted_index_command_form() {
        let mut args = base_args();
        apply_command_form(&mut args, "search index \"./docs with spaces\"")
            .expect("quoted index command should parse");
        assert_eq!(args.action, SearchAction::Index);
        assert_eq!(args.path.as_deref(), Some("./docs with spaces"));
    }

    #[test]
    fn parses_quoted_query_command_form() {
        let mut args = base_args();
        apply_command_form(&mut args, "search query \"concurrency bug\"")
            .expect("quoted query command should parse");
        assert_eq!(args.action, SearchAction::Query);
        assert_eq!(args.query.as_deref(), Some("concurrency bug"));
    }

    #[test]
    fn chunks_long_text() {
        let content = "a".repeat(6000);
        let chunks = chunk_text(content.as_str());
        assert!(chunks.len() >= 2);
        assert!(chunks.iter().all(|chunk| !chunk.is_empty()));
    }

    #[test]
    fn search_mode_serializes_as_lowercase_string() {
        let value = serde_json::to_value(vec![SearchMode::Fulltext, SearchMode::Regex])
            .expect("search modes should serialize");
        assert_eq!(value, serde_json::json!(["fulltext", "regex"]));
    }

    #[tokio::test]
    async fn resolves_relative_query_path_against_sandbox() {
        let temp = tempfile::tempdir().expect("tempdir");
        let sandbox_dir = temp.path().join("session");
        let indexed_dir = sandbox_dir.join("data").join("semantic-test");
        std::fs::create_dir_all(&indexed_dir).expect("create indexed dir");
        std::fs::write(indexed_dir.join("note.md"), "hello").expect("write note");

        let resolved = resolve_search_path(sandbox_dir.as_path(), "data/semantic-test")
            .await
            .expect("resolve relative path");

        assert_eq!(
            resolved,
            std::fs::canonicalize(&indexed_dir).expect("canonical indexed dir")
        );
    }
}
