//! Tests of `may-client` internals that exercise root-only agent/shell types.

#![cfg(test)]

#[cfg(test)]
mod issue_github {
    use std::sync::{Arc, Mutex};

    use crate::issue::github::{
        GitHubIssueStore, GitHubRest, GitHubTokenSource, RepositoryAccess, next_page_url,
    };
    use crate::issue::{IssueError, IssueKind, IssueListState, IssueRelationKind, IssueStore};
    use crate::workspace::GitHubRepository;
    use may_protocol::test_support::ApiReply;

    #[derive(Debug, Clone)]
    struct StaticToken;

    impl GitHubTokenSource for StaticToken {
        fn token(&self) -> impl Future<Output = Result<String, IssueError>> + Send {
            std::future::ready(Ok("test-token".to_string()))
        }
    }

    type Requests = may_protocol::test_support::ApiRequests;

    /// Spin up a mock GitHub API on a loopback port. `handler` receives the
    /// request method, path and body and returns `(status, json_body)`.
    async fn mock_api(
        handler: impl Fn(&str, &str, &str) -> (u16, serde_json::Value) + Send + Sync + 'static,
    ) -> (String, Requests) {
        mock_paged_api(move |method, path, body| {
            let (status, json) = handler(method, path, body);
            (status, json, None)
        })
        .await
    }

    /// Like [`mock_api`]; the handler may also name the path of the next
    /// page, sent as a `Link: <…>; rel="next"` header.
    async fn mock_paged_api(
        handler: impl Fn(&str, &str, &str) -> (u16, serde_json::Value, Option<String>)
        + Send
        + Sync
        + 'static,
    ) -> (String, Requests) {
        may_protocol::test_support::serve_api(move |request, base| {
            let (status, json, next) = handler(&request.method, &request.path, &request.body);
            let reply = ApiReply::json(status, json.to_string());
            match next {
                Some(next) => reply.header("link", format!("<{base}{next}>; rel=\"next\"")),
                None => reply,
            }
        })
        .await
    }

    fn gh_issue_json(number: i64, title: &str, state: &str) -> serde_json::Value {
        serde_json::json!({
            "id": number * 100,
            "number": number,
            "title": title,
            "body": "body",
            "state": state,
            "labels": [],
            "repository_url": "https://api.github.com/repos/o/r",
            "created_at": "2026-10-01T00:00:00Z",
            "updated_at": "2026-10-02T00:00:00Z",
        })
    }

    fn store(api_base: &str) -> GitHubIssueStore<StaticToken> {
        GitHubIssueStore::with_api_base("o/r".parse().expect("repository"), StaticToken, api_base)
    }

    #[tokio::test]
    async fn a_subagent_reads_github_issues_and_sends_no_write_request() {
        use crate::agent::issue_tool::{SubagentIssueTool, subagent_checks};

        let (api_base, requests) = mock_api(|method, path, _body| match (method, path) {
            ("GET", "/repos/o/r/issues/1") => (200, gh_issue_json(1, "seed", "open")),
            ("GET", path) if path.starts_with("/repos/o/r/issues?") => {
                (200, serde_json::json!([gh_issue_json(1, "seed", "open")]))
            }
            (
                "GET",
                "/repos/o/r/issues/1/comments?per_page=100"
                | "/repos/o/r/issues/1/sub_issues?per_page=100"
                | "/repos/o/r/issues/1/dependencies/blocked_by?per_page=100",
            ) => (200, serde_json::json!([])),
            _ => (500, serde_json::json!({"message": "unexpected request"})),
        })
        .await;
        let dir = tempfile::tempdir().expect("tempdir");
        let db = may_db::ChatDb::open(&dir.path().join("history.db"))
            .await
            .expect("chat db should open");
        let tool = SubagentIssueTool::new(
            store(&api_base),
            crate::issue::provenance::IssueProvenance::new(db, "test".to_string()),
        );

        subagent_checks::assert_reads_work(&tool, 1).await;
        let reads = requests.lock().expect("requests").len();
        assert!(reads > 0, "the reads reached the API");
        subagent_checks::assert_writes_refused(&tool).await;

        let sent = requests.lock().expect("requests").clone();
        assert_eq!(sent.len(), reads, "refused writes sent no request");
        assert!(sent.iter().all(|request| request.method == "GET"));
    }

    /// `--repo OWNER/REPO` reads another GitHub repository's issues while the
    /// session's own backend (a may store) is untouched, and a write ignores it.
    #[tokio::test]
    async fn view_and_list_read_another_github_repository_through_repo() {
        use crate::agent::issue_tool::{IssueArgs, IssueTool};
        use crate::issue::IssueTargets;
        use aither_core::llm::{Tool, ToolContext};

        let (api_base, requests) = mock_api(|method, path, _body| match (method, path) {
            ("GET", "/repos/x/y/issues/7") => (200, gh_issue_json(7, "elsewhere", "open")),
            ("GET", path) if path.starts_with("/repos/x/y/issues?") => (
                200,
                serde_json::json!([gh_issue_json(7, "elsewhere", "open")]),
            ),
            (
                "GET",
                "/repos/x/y/issues/7/comments?per_page=100"
                | "/repos/x/y/issues/7/sub_issues?per_page=100"
                | "/repos/x/y/issues/7/dependencies/blocked_by?per_page=100",
            ) => (200, serde_json::json!([])),
            _ => (500, serde_json::json!({"message": "unexpected request"})),
        })
        .await;
        let dir = tempfile::tempdir().expect("tempdir");
        let db = may_db::ChatDb::open(&dir.path().join("history.db"))
            .await
            .expect("chat db should open");
        let session_store = crate::issue::MayIssueStore::new(db.clone(), "session".to_string());
        session_store
            .create("local only", "", &[])
            .await
            .expect("local issue");
        let targets = IssueTargets::new(
            db.clone(),
            dir.path().to_path_buf(),
            dir.path().join("workspace.toml"),
            std::sync::Arc::new(may_config::config::MayConfig::default()),
            GitHubRest::with_api_base(StaticToken, &api_base),
            may_config::forge::LiveForgeAuth::default(),
        );
        let tool = IssueTool::new(
            session_store,
            crate::issue::provenance::IssueProvenance::new(db, "test".to_string()),
        )
        .with_targets(targets);

        let viewed = tool
            .call(
                IssueArgs::View {
                    number: 7,
                    repo: Some("x/y".to_string()),
                },
                ToolContext::new(),
            )
            .await
            .expect("view another repository's issue");
        assert!(format!("{viewed:?}").contains("elsewhere"), "{viewed:?}");
        let listed = tool
            .call(
                IssueArgs::List {
                    state: IssueListState::Open,
                    labels: Vec::new(),
                    limit: 10,
                    repo: Some("x/y".to_string()),
                },
                ToolContext::new(),
            )
            .await
            .expect("list another repository's issues");
        let listed = format!("{listed:?}");
        assert!(listed.contains("elsewhere") && !listed.contains("local only"));
        let local = tool
            .call(
                IssueArgs::List {
                    state: IssueListState::Open,
                    labels: Vec::new(),
                    limit: 10,
                    repo: None,
                },
                ToolContext::new(),
            )
            .await
            .expect("list the session's issues");
        let local = format!("{local:?}");
        assert!(local.contains("local only") && !local.contains("elsewhere"));
        let sent = requests.lock().expect("requests").clone();
        assert!(sent.iter().all(|request| request.method == "GET"));
        assert!(
            sent.iter()
                .all(|request| request.path.starts_with("/repos/x/y/"))
        );

        let error = tool
            .call(
                IssueArgs::View {
                    number: 7,
                    repo: Some("not a repository".to_string()),
                },
                ToolContext::new(),
            )
            .await
            .expect_err("a malformed target is refused");
        assert!(error.to_string().contains("--repo"), "{error}");
    }

    #[tokio::test]
    async fn create_get_and_relations_hit_github_api() {
        let (api_base, requests) = mock_api(|method, path, body| match (method, path) {
            ("POST", "/repos/o/r/labels") => (201, serde_json::json!({"name": "bug"})),
            ("POST", "/repos/o/r/issues") => (201, gh_issue_json(1, "created", "open")),
            ("GET", "/repos/o/r/issues/1") => (200, gh_issue_json(1, "created", "open")),
            ("GET", "/repos/o/r/issues/2") => (200, gh_issue_json(2, "blocker", "open")),
            (
                "GET",
                "/repos/o/r/issues/1/comments?per_page=100"
                | "/repos/o/r/issues/1/sub_issues?per_page=100"
                | "/repos/o/r/issues/1/dependencies/blocked_by?per_page=100",
            ) => (200, serde_json::json!([])),
            ("POST", "/repos/o/r/issues/1/dependencies/blocked_by") => {
                assert!(body.contains("\"issue_id\":200"), "{body}");
                (201, gh_issue_json(1, "created", "open"))
            }
            _ => panic!("unexpected request {method} {path}"),
        })
        .await;
        let store = store(&api_base);

        let created = store
            .create("created", "body", &["bug".to_string()])
            .await
            .expect("create");
        assert_eq!(created.number, 1);

        let detail = store
            .edit_relation(1, IssueRelationKind::BlockedBy, 2, true)
            .await
            .expect("blocked-by add");
        assert_eq!(detail.number, 1);

        {
            let requests = requests.lock().expect("requests");
            assert!(
                requests
                    .iter()
                    .all(|request| request.authorization == "Bearer test-token"),
                "every request must carry the token: {requests:?}"
            );
            assert!(
                requests
                    .iter()
                    .all(|request| !request.body.contains("test-token")),
                "token must never appear in a request body"
            );
            assert!(requests.iter().any(|request| {
                request.method == "POST"
                    && request.path == "/repos/o/r/issues/1/dependencies/blocked_by"
            }));
            drop(requests);
        }
    }

    #[tokio::test]
    async fn changes_to_github_issues_are_recorded_with_their_session_agent_and_call() {
        #[derive(Default)]
        struct Remote {
            title: String,
            state: String,
            comments: Vec<serde_json::Value>,
        }
        let remote = Arc::new(Mutex::new(Remote::default()));
        let handler_remote = remote.clone();
        let (api_base, _requests) = mock_api(move |method, path, body| {
            let mut remote = handler_remote.lock().expect("remote");
            let issue = |remote: &Remote| gh_issue_json(2, &remote.title, &remote.state);
            match (method, path) {
                ("POST", "/repos/o/r/issues") => {
                    remote.title = "first".to_string();
                    remote.state = "open".to_string();
                    (201, issue(&remote))
                }
                ("PATCH", "/repos/o/r/issues/2") => {
                    let patch: serde_json::Value = serde_json::from_str(body).expect("patch");
                    if let Some(title) = patch["title"].as_str() {
                        remote.title = title.to_string();
                    }
                    if let Some(state) = patch["state"].as_str() {
                        remote.state = state.to_string();
                    }
                    (200, issue(&remote))
                }
                ("GET", "/repos/o/r/issues/2") => (200, issue(&remote)),
                ("GET", "/repos/o/r/issues/1") => (200, gh_issue_json(1, "outside", "open")),
                ("POST", "/repos/o/r/issues/2/comments") => {
                    let comment = serde_json::json!({
                        "id": 7001,
                        "body": "hello",
                        "created_at": "2026-10-03T00:00:00Z",
                        "user": null,
                    });
                    remote.comments.push(comment.clone());
                    (201, comment)
                }
                ("GET", "/repos/o/r/issues/2/comments?per_page=100") => {
                    (200, serde_json::Value::Array(remote.comments.clone()))
                }
                (
                    "GET",
                    "/repos/o/r/issues/1/comments?per_page=100"
                    | "/repos/o/r/issues/1/sub_issues?per_page=100"
                    | "/repos/o/r/issues/1/dependencies/blocked_by?per_page=100"
                    | "/repos/o/r/issues/2/sub_issues?per_page=100"
                    | "/repos/o/r/issues/2/dependencies/blocked_by?per_page=100",
                ) => (200, serde_json::json!([])),
                _ => (500, serde_json::json!({"message": "unexpected request"})),
            }
        })
        .await;
        let dir = tempfile::tempdir().expect("tempdir");
        let db = may_db::ChatDb::open(&dir.path().join("history.db"))
            .await
            .expect("chat db should open");
        crate::agent::issue_tool::provenance_checks::assert_changes_are_recorded(
            &db,
            store(&api_base),
            2,
            1,
        )
        .await;
    }

    #[tokio::test]
    async fn removing_a_sub_issue_uses_githubs_sub_issue_endpoint() {
        let (api_base, requests) = mock_api(|method, path, body| match (method, path) {
            ("GET", "/repos/o/r/issues/1") => (200, gh_issue_json(1, "parent", "open")),
            ("GET", "/repos/o/r/issues/2") => (200, gh_issue_json(2, "child", "open")),
            (
                "GET",
                "/repos/o/r/issues/1/comments?per_page=100"
                | "/repos/o/r/issues/1/sub_issues?per_page=100"
                | "/repos/o/r/issues/1/dependencies/blocked_by?per_page=100",
            ) => (200, serde_json::json!([])),
            ("DELETE", "/repos/o/r/issues/1/sub_issue") => {
                assert!(body.contains("\"sub_issue_id\":200"), "{body}");
                (200, gh_issue_json(1, "parent", "open"))
            }
            _ => (404, serde_json::json!({"message": "Not Found"})),
        })
        .await;

        store(&api_base)
            .edit_relation(1, IssueRelationKind::SubIssue, 2, false)
            .await
            .expect("sub-issue removal");

        let sent = requests.lock().expect("requests").clone();
        assert!(
            sent.iter().any(|request| request.method == "DELETE"
                && request.path == "/repos/o/r/issues/1/sub_issue"),
            "{sent:?}"
        );
    }

    #[tokio::test]
    async fn list_filters_pull_requests() {
        let mut pr = gh_issue_json(4, "a pr", "open");
        pr["pull_request"] = serde_json::json!({"url": "https://example/pr"});
        let issues_json = serde_json::json!([gh_issue_json(3, "real issue", "open"), pr]);
        let (api_base, _requests) = mock_api(move |method, path, _body| match (method, path) {
            ("GET", path) if path.starts_with("/repos/o/r/issues?") => (200, issues_json.clone()),
            _ => panic!("unexpected request {method} {path}"),
        })
        .await;
        let issues = store(&api_base)
            .list(IssueListState::All, &[], 50)
            .await
            .expect("list");
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].number, 3);
    }

    #[tokio::test]
    async fn list_encodes_labels_in_the_query() {
        let (api_base, requests) = mock_api(|method, path, _body| match (method, path) {
            ("GET", path) if path.starts_with("/repos/o/r/issues?") => {
                (200, serde_json::json!([gh_issue_json(1, "one", "open")]))
            }
            _ => panic!("unexpected request {method} {path}"),
        })
        .await;
        let labels = ["good first issue", "C++", "a&b"].map(String::from);
        store(&api_base)
            .list(IssueListState::Open, &labels, 10)
            .await
            .expect("list");
        let sent = requests.lock().expect("requests").clone();
        assert_eq!(sent.len(), 1);
        assert_eq!(
            sent[0].path,
            "/repos/o/r/issues?state=open&per_page=10&labels=good+first+issue%2CC%2B%2B%2Ca%26b"
        );
    }

    #[tokio::test]
    async fn list_reads_the_next_page_when_pull_requests_fill_the_first() {
        let mut pr = gh_issue_json(2, "a pr", "open");
        pr["pull_request"] = serde_json::json!({"url": "https://example/pr"});
        let first = serde_json::json!([gh_issue_json(1, "one", "open"), pr]);
        let second = serde_json::json!([
            gh_issue_json(3, "three", "open"),
            gh_issue_json(4, "four", "open")
        ]);
        let (api_base, requests) =
            mock_paged_api(move |method, path, _body| match (method, path) {
                ("GET", "/repos/o/r/issues?page=2") => (200, second.clone(), None),
                ("GET", path) if path.starts_with("/repos/o/r/issues?") => (
                    200,
                    first.clone(),
                    Some("/repos/o/r/issues?page=2".to_string()),
                ),
                _ => panic!("unexpected request {method} {path}"),
            })
            .await;
        let issues = store(&api_base)
            .list(IssueListState::All, &[], 2)
            .await
            .expect("list");
        assert_eq!(
            issues.iter().map(|issue| issue.number).collect::<Vec<_>>(),
            [1, 3]
        );
        assert_eq!(requests.lock().expect("requests").len(), 2);
    }

    /// Hands out `token-1`, `token-2`, … and counts the reads.
    #[derive(Debug, Clone, Default)]
    struct CountingToken(Arc<std::sync::atomic::AtomicUsize>);

    impl GitHubTokenSource for CountingToken {
        fn token(&self) -> impl Future<Output = Result<String, IssueError>> + Send {
            let read = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            std::future::ready(Ok(format!("token-{read}")))
        }
    }

    #[tokio::test]
    async fn the_token_is_read_once_and_again_only_after_a_401() {
        let rejected = std::sync::atomic::AtomicBool::new(false);
        let (api_base, requests) = mock_api(move |_method, _path, _body| {
            if rejected.swap(true, std::sync::atomic::Ordering::SeqCst) {
                (200, serde_json::json!({"login": "me"}))
            } else {
                (401, serde_json::json!({"message": "Bad credentials"}))
            }
        })
        .await;
        let tokens = CountingToken::default();
        let rest = GitHubRest::with_api_base(tokens.clone(), &api_base);
        for _ in 0..3 {
            assert_eq!(rest.viewer_login().await.expect("login"), "me");
        }
        assert_eq!(tokens.0.load(std::sync::atomic::Ordering::SeqCst), 2);
        let auth: Vec<_> = requests
            .lock()
            .expect("requests")
            .iter()
            .map(|request| request.authorization.clone())
            .collect();
        assert_eq!(
            auth,
            [
                "Bearer token-1",
                "Bearer token-2",
                "Bearer token-2",
                "Bearer token-2"
            ]
        );
    }

    fn issue_at(number: i64, updated_at: &str) -> serde_json::Value {
        let mut issue = gh_issue_json(number, &format!("issue {number}"), "open");
        issue["updated_at"] = serde_json::json!(updated_at);
        issue
    }

    fn pull_request_at(number: i64, updated_at: &str) -> serde_json::Value {
        let mut pr = issue_at(number, updated_at);
        pr["pull_request"] = serde_json::json!({"url": "https://example/pr"});
        pr
    }

    #[tokio::test]
    async fn comments_are_read_across_pages() {
        let comment = |body: &str| {
            serde_json::json!({
                "id": 1,
                "user": {"login": "someone"},
                "body": body,
                "created_at": "2026-10-01T00:00:00Z",
            })
        };
        let (api_base, requests) =
            mock_paged_api(move |method, path, _body| match (method, path) {
                ("GET", "/repos/o/r/issues/1") => (200, gh_issue_json(1, "seed", "open"), None),
                ("GET", "/repos/o/r/issues/1/comments?per_page=100") => (
                    200,
                    serde_json::json!([comment("first"), comment("second")]),
                    Some("/comments-2".to_string()),
                ),
                ("GET", "/comments-2") => (200, serde_json::json!([comment("third")]), None),
                (
                    "GET",
                    "/repos/o/r/issues/1/sub_issues?per_page=100"
                    | "/repos/o/r/issues/1/dependencies/blocked_by?per_page=100",
                ) => (200, serde_json::json!([]), None),
                _ => panic!("unexpected request {method} {path}"),
            })
            .await;

        let detail = store(&api_base).get(1).await.expect("get");

        let bodies: Vec<_> = detail.comments.iter().map(|c| c.body.as_str()).collect();
        assert_eq!(bodies, ["first", "second", "third"]);
        let paths: Vec<_> = requests
            .lock()
            .expect("requests")
            .iter()
            .map(|request| request.path.clone())
            .collect();
        assert!(paths.contains(&"/comments-2".to_string()), "{paths:?}");
    }

    /// Pages of `/issues` as the sync sees them: a page of only pull
    /// requests, then two issues sharing an update time, then a later one.
    fn sync_pages(path: &str) -> (u16, serde_json::Value, Option<String>) {
        const T1: &str = "2026-10-01T00:00:00Z";
        const T2: &str = "2026-10-02T00:00:00Z";
        const T3: &str = "2026-10-03T00:00:00Z";
        if path.starts_with("/repos/o/r/issues?") {
            (
                200,
                serde_json::json!([pull_request_at(10, T1), pull_request_at(11, T1)]),
                Some("/page-2".to_string()),
            )
        } else if path == "/page-2" {
            (
                200,
                serde_json::json!([issue_at(1, T2), issue_at(2, T2)]),
                Some("/page-3".to_string()),
            )
        } else if path == "/page-3" {
            (200, serde_json::json!([issue_at(3, T3)]), None)
        } else {
            (404, serde_json::json!({"message": "Not Found"}), None)
        }
    }

    #[tokio::test]
    async fn the_sync_listing_follows_pages_past_pull_requests_and_keeps_ties_whole() {
        let (api_base, _requests) = mock_paged_api(|_method, path, _body| sync_pages(path)).await;

        let updated = store(&api_base)
            .updated_since(None, 1)
            .await
            .expect("updated_since");

        // The pull-request-only first page is passed over, and issue 2 comes
        // with issue 1 although the limit of 1 is reached at issue 1.
        let numbers: Vec<_> = updated.iter().map(|issue| issue.number).collect();
        assert_eq!(numbers, [1, 2]);
    }

    #[tokio::test]
    async fn the_search_sync_indexes_issues_behind_a_pull_request_page() {
        use crate::issue::search::{IssueSearch, collection_name};
        use crate::repo_search::backend::testing::MemoryCollections;
        use may_db::ChatDb;

        let (api_base, _requests) = mock_paged_api(|method, path, _body| {
            if let Some(number) = path
                .strip_prefix("/repos/o/r/issues/")
                .and_then(|rest| rest.parse::<i64>().ok())
            {
                let updated_at = match number {
                    1 | 2 => "2026-10-02T00:00:00Z",
                    _ => "2026-10-03T00:00:00Z",
                };
                return (200, issue_at(number, updated_at), None);
            }
            if path.contains("/comments")
                || path.contains("/sub_issues")
                || path.contains("/blocked_by")
            {
                return (200, serde_json::json!([]), None);
            }
            assert_eq!(method, "GET");
            if path.contains("since=") {
                // Inclusive: the newest issue comes back and is skipped.
                return (
                    200,
                    serde_json::json!([issue_at(3, "2026-10-03T00:00:00Z")]),
                    None,
                );
            }
            sync_pages(path)
        })
        .await;
        let dir = tempfile::tempdir().expect("temp dir");
        let db = ChatDb::open(&dir.path().join("history.db"))
            .await
            .expect("open db");
        let store = store(&api_base);
        let search = IssueSearch::new(db.clone(), MemoryCollections::default());

        search.sync(&store).await.expect("sync");

        let collection = collection_name(&store.search_key());
        for number in [1, 2, 3] {
            assert!(
                db.issue_search_doc(&collection, number)
                    .await
                    .expect("doc")
                    .is_some(),
                "issue {number} is indexed"
            );
        }
        assert_eq!(
            db.issue_search_watermark(&collection)
                .await
                .expect("watermark")
                .as_deref(),
            Some("2026-10-03T00:00:00Z")
        );

        search.sync(&store).await.expect("second sync");
    }

    #[test]
    fn the_next_page_is_read_from_the_link_header() {
        let link = "<https://api.github.com/x?labels=a,b&page=2>; rel=\"next\", \
                    <https://api.github.com/x?page=9>; rel=\"last\"";
        assert_eq!(
            next_page_url(link).as_deref(),
            Some("https://api.github.com/x?labels=a,b&page=2")
        );
        assert_eq!(
            next_page_url("<https://api.github.com/x?page=1>; rel=\"prev\""),
            None
        );
    }

    /// A mock API where the authenticated user is `me` and repository `o/r`
    /// has the given owner and admin permission; issue 5 was opened by
    /// `thread_author`.
    async fn ownership_api(
        owner: &'static str,
        admin: bool,
        thread_author: &'static str,
    ) -> String {
        mock_api(move |method, path, _body| match (method, path) {
            ("GET", "/user") => (200, serde_json::json!({"login": "me"})),
            ("GET", "/repos/o/r") => (
                200,
                serde_json::json!({"owner": {"login": owner}, "permissions": {"admin": admin}}),
            ),
            ("GET", "/repos/o/r/issues/5") => {
                let mut issue = gh_issue_json(5, "thread", "open");
                issue["user"] = serde_json::json!({"login": thread_author});
                (200, issue)
            }
            _ => panic!("unexpected request {method} {path}"),
        })
        .await
        .0
    }

    /// A mock API where the authenticated user is `me` and `GET /repos/o/r`
    /// answers `repo`.
    async fn access_api(repo: serde_json::Value) -> String {
        mock_api(move |method, path, _body| match (method, path) {
            ("GET", "/user") => (200, serde_json::json!({"login": "me"})),
            ("GET", "/repos/o/r") => (200, repo.clone()),
            _ => panic!("unexpected request {method} {path}"),
        })
        .await
        .0
    }

    fn rest(api_base: &str) -> GitHubRest<StaticToken> {
        GitHubRest::with_api_base(StaticToken, api_base)
    }

    #[tokio::test]
    async fn repository_access_reports_ownership_push_and_issues() {
        let repository: GitHubRepository = "o/r".parse().expect("repository");
        for (repo, expected) in [
            // Owned with issues.
            (
                serde_json::json!({"owner": {"login": "me"}}),
                RepositoryAccess {
                    owned: true,
                    can_push: true,
                    has_issues: true,
                },
            ),
            // An owned fork with Issues disabled.
            (
                serde_json::json!({"owner": {"login": "me"}, "has_issues": false}),
                RepositoryAccess {
                    owned: true,
                    can_push: true,
                    has_issues: false,
                },
            ),
            // A collaborator: not owned, push permission.
            (
                serde_json::json!({"owner": {"login": "org"}, "permissions": {"push": true}}),
                RepositoryAccess {
                    owned: false,
                    can_push: true,
                    has_issues: true,
                },
            ),
            // Admin permission counts as owned.
            (
                serde_json::json!({"owner": {"login": "org"}, "permissions": {"admin": true}}),
                RepositoryAccess {
                    owned: true,
                    can_push: true,
                    has_issues: true,
                },
            ),
            // Read-only third-party repository.
            (
                serde_json::json!({"owner": {"login": "org"}, "permissions": {"push": false}}),
                RepositoryAccess {
                    owned: false,
                    can_push: false,
                    has_issues: true,
                },
            ),
        ] {
            let api_base = access_api(repo).await;
            assert_eq!(
                rest(&api_base)
                    .repository_access(&repository)
                    .await
                    .expect("access"),
                expected,
            );
        }
    }

    #[tokio::test]
    async fn a_failed_access_lookup_is_an_error() {
        let repository: GitHubRepository = "o/r".parse().expect("repository");
        let (api_base, _requests) = mock_api(|_method, path, _body| match path {
            "/user" => (200, serde_json::json!({"login": "me"})),
            _ => (500, serde_json::json!({"message": "boom"})),
        })
        .await;
        assert!(
            rest(&api_base)
                .repository_access(&repository)
                .await
                .is_err()
        );
        let (api_base, _requests) =
            mock_api(|_method, _path, _body| (404, serde_json::json!({"message": "Not Found"})))
                .await;
        assert!(
            rest(&api_base)
                .repository_access(&repository)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_repository_is_owned_by_its_owner_login_or_admin_permission() {
        for (owner, admin, owned) in [
            ("me", false, true),
            ("ME", false, true),
            ("org", true, true),
            ("org", false, false),
        ] {
            let api_base = ownership_api(owner, admin, "x").await;
            assert_eq!(
                store(&api_base).owned_by_user().await.expect("lookup"),
                owned,
                "owner {owner} admin {admin}"
            );
        }
    }

    #[tokio::test]
    async fn a_failed_ownership_lookup_is_an_error_never_owned() {
        let (api_base, _requests) = mock_api(|_method, path, _body| match path {
            "/user" => (200, serde_json::json!({"login": "me"})),
            _ => (500, serde_json::json!({"message": "boom"})),
        })
        .await;
        assert!(store(&api_base).owned_by_user().await.is_err());
        let (api_base, _requests) =
            mock_api(|_method, _path, _body| (404, serde_json::json!({"message": "Not Found"})))
                .await;
        assert!(store(&api_base).owned_by_user().await.is_err());
    }

    #[tokio::test]
    async fn a_thread_is_the_users_when_their_account_opened_it() {
        let api_base = ownership_api("org", false, "me").await;
        assert!(store(&api_base).opened_by_user(5).await.expect("lookup"));
        let api_base = ownership_api("org", false, "someone").await;
        assert!(!store(&api_base).opened_by_user(5).await.expect("lookup"));
    }

    fn gate(timeout: std::time::Duration) -> crate::agent::publish_approval::PublishGate {
        crate::agent::publish_approval::PublishGate::new(
            Arc::new(arc_swap::ArcSwap::from_pointee(
                may_config::config::MayConfig::default(),
            )),
            crate::agent::permission::PermissionApprover::new()
                .with_approval_timeout(timeout)
                .for_session("session-1"),
        )
    }

    #[tokio::test]
    async fn the_gate_holds_publishing_on_an_external_repository() {
        let short = std::time::Duration::from_millis(60);
        let external = store(&ownership_api("org", false, "someone").await);
        let gate = gate(short);

        for kind in [IssueKind::Bug, IssueKind::Feature, IssueKind::Performance] {
            let error = gate
                .approve_issue(&external, kind, "t", &[], "b")
                .await
                .expect_err("an unanswered external issue is denied");
            assert!(matches!(error, IssueError::ApprovalDenied(_)), "{error:?}");
        }
        gate.approve_comment(&external, 5, "hi")
            .await
            .expect_err("an unanswered external comment is denied");
        let repository: GitHubRepository = "o/r".parse().expect("repository");
        gate.approve_pull_request(&external.rest, &repository, "t", "b")
            .await
            .expect_err("an unanswered external pull request is denied");
    }

    #[tokio::test]
    async fn the_gate_lets_the_user_comment_on_their_own_thread_of_an_external_repository() {
        let own_thread = store(&ownership_api("org", false, "me").await);
        gate(std::time::Duration::from_secs(30))
            .approve_comment(&own_thread, 5, "hi")
            .await
            .expect("no prompt for a thread the user's account opened");
    }

    #[tokio::test]
    async fn the_gate_asks_nothing_for_an_owned_repository_beyond_the_kind_entry() {
        let owned = store(&ownership_api("me", false, "someone").await);
        let slow = gate(std::time::Duration::from_secs(30));
        let repository: GitHubRepository = "o/r".parse().expect("repository");
        let quick = std::time::Duration::from_millis(500);
        tokio::time::timeout(quick, async {
            slow.approve_issue(&owned, IssueKind::Bug, "t", &[], "b")
                .await
                .expect("bug");
            slow.approve_issue(&owned, IssueKind::Performance, "t", &[], "b")
                .await
                .expect("performance");
            slow.approve_comment(&owned, 5, "hi")
                .await
                .expect("comment");
            slow.approve_pull_request(&owned.rest, &repository, "t", "b")
                .await
                .expect("pull request");
        })
        .await
        .expect("nothing was asked");
        // A feature issue on an owned repository is approved when nobody answers.
        gate(std::time::Duration::from_millis(60))
            .approve_issue(&owned, IssueKind::Feature, "t", &[], "b")
            .await
            .expect("auto approves");
    }

    #[tokio::test]
    async fn missing_issue_maps_to_not_found() {
        let (api_base, _requests) =
            mock_api(|_method, _path, _body| (404, serde_json::json!({"message": "Not Found"})))
                .await;
        assert!(matches!(
            store(&api_base).get(99).await,
            Err(IssueError::NotFound(99))
        ));
    }
}

#[cfg(test)]
mod repo_clone {
    use std::path::{Path, PathBuf};

    use may_db::ChatDb;

    use crate::repo_clone::{SessionRepos, uncommitted_changes};
    use crate::test_sandbox;

    fn git(dir: Option<&Path>, args: &[&str]) -> String {
        may_config::host_git::HostGit::from_file("/dev/null")
            .output(dir, args)
            .unwrap()
    }

    fn init_repo(dir: &Path) {
        git(None, &["init", "-b", "main", &dir.to_string_lossy()]);
        git(Some(dir), &["config", "user.email", "t@t"]);
        git(Some(dir), &["config", "user.name", "t"]);
        std::fs::write(dir.join("file.txt"), "hello").unwrap();
        git(Some(dir), &["add", "."]);
        git(Some(dir), &["commit", "-m", "init"]);
    }

    fn repo_key(dir: &Path) -> PathBuf {
        dunce::canonicalize(dir.join(".git")).unwrap()
    }

    async fn session_repos(tmp: &Path) -> SessionRepos {
        let db = ChatDb::open(&tmp.join("history.db")).await.unwrap();
        SessionRepos::new(&tmp.join("session"), "s1", db)
    }

    fn staging(tmp: &Path) -> PathBuf {
        tmp.join("staging")
    }

    #[tokio::test]
    async fn uncommitted_changes_lists_dirty_and_untracked() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp = dunce::canonicalize(tmp.path()).unwrap();
        let checkout = tmp.join("checkout");
        init_repo(&checkout);
        let repos = session_repos(&tmp).await;
        let clone = repos
            .ensure_local_clone(&checkout, &repo_key(&checkout), &staging(&tmp))
            .await
            .unwrap();
        let terminal = test_sandbox::TestTerminal::new(repos.session_root()).await;
        std::fs::write(clone.join("file.txt"), "dirty").unwrap();
        std::fs::write(clone.join("new.txt"), "untracked").unwrap();
        let changes = uncommitted_changes(&terminal.sandbox(), &clone)
            .await
            .unwrap();
        assert!(changes.iter().any(|l| l.contains("file.txt")));
        assert!(changes.iter().any(|l| l.contains("new.txt")));
    }

    /// A clone an agent armed with hostile configuration — fsmonitor,
    /// filters, hooks, a credential helper — is still judged by host git
    /// that honors none of it: nothing the clone configures executes, and
    /// the untouched clone is reused.
    #[tokio::test]
    async fn a_hostile_clone_configuration_runs_nothing_during_reuse() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp = dunce::canonicalize(tmp.path()).unwrap();
        let checkout = tmp.join("checkout");
        init_repo(&checkout);
        let repos = session_repos(&tmp).await;
        let clone = repos
            .ensure_local_clone(&checkout, &repo_key(&checkout), &staging(&tmp))
            .await
            .unwrap();
        let host =
            test_sandbox::arm_hostile_config(repos.session_root(), &clone, &tmp.join("outside"));
        let again = repos
            .ensure_local_clone(&checkout, &repo_key(&checkout), &staging(&tmp))
            .await
            .unwrap();
        assert_eq!(clone, again);
        let fired = test_sandbox::markers(&host);
        assert!(fired.is_empty(), "hostile configuration ran: {fired:?}");
    }
}
