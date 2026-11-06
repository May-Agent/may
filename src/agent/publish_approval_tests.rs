use std::time::Duration;

use super::*;

const SHORT: Duration = Duration::from_millis(60);

fn issue(kind: IssueKind) -> Publication<'static> {
    Publication::Issue {
        kind,
        title: "title",
        labels: &[],
        body: "body",
    }
}

const PULL_REQUEST: Publication<'static> = Publication::PullRequest {
    title: "title",
    body: "body",
};

const COMMENT: Publication<'static> = Publication::Comment {
    number: 7,
    body: "body",
};

fn gate_with(config: IssueApprovalConfig, timeout: Duration) -> PublishGate {
    let may = MayConfig {
        issue_approval: config,
        ..MayConfig::default()
    };
    PublishGate::new(
        Arc::new(ArcSwap::from_pointee(may)),
        PermissionApprover::new()
            .with_approval_timeout(timeout)
            .for_session("session-1"),
    )
}

#[test]
fn the_defaults_prompt_where_the_issue_says() {
    let config = IssueApprovalConfig::default();
    for (publication, external, expected) in [
        (issue(IssueKind::Bug), false, ApprovalPolicy::None),
        (issue(IssueKind::Performance), false, ApprovalPolicy::None),
        (issue(IssueKind::Feature), false, ApprovalPolicy::Auto),
        (issue(IssueKind::Bug), true, ApprovalPolicy::Required),
        (
            issue(IssueKind::Performance),
            true,
            ApprovalPolicy::Required,
        ),
        (issue(IssueKind::Feature), true, ApprovalPolicy::Required),
        (PULL_REQUEST, true, ApprovalPolicy::Required),
        (COMMENT, true, ApprovalPolicy::Required),
    ] {
        assert_eq!(
            publication.policy(&config, external),
            expected,
            "{publication:?} external={external}"
        );
    }
}

#[test]
fn pull_requests_and_comments_on_an_owned_repository_have_no_entry() {
    // Even the strictest settings cannot make an owned repository prompt.
    let strict = IssueApprovalConfig {
        external_issue: ApprovalPolicy::Required,
        external_pull_request: ApprovalPolicy::Required,
        external_comment: ApprovalPolicy::Required,
        bug: ApprovalPolicy::Required,
        feature: ApprovalPolicy::Required,
        performance: ApprovalPolicy::Required,
    };
    assert_eq!(PULL_REQUEST.policy(&strict, false), ApprovalPolicy::None);
    assert_eq!(COMMENT.policy(&strict, false), ApprovalPolicy::None);
}

#[test]
fn an_external_issue_is_held_to_the_stricter_entry() {
    let config = IssueApprovalConfig {
        external_issue: ApprovalPolicy::Auto,
        bug: ApprovalPolicy::Required,
        ..IssueApprovalConfig::default()
    };
    assert_eq!(
        issue(IssueKind::Bug).policy(&config, true),
        ApprovalPolicy::Required
    );
    assert_eq!(
        issue(IssueKind::Performance).policy(&config, true),
        ApprovalPolicy::Auto
    );
}

#[test]
fn the_request_shows_repository_title_labels_and_body() {
    let labels = ["area: harness".to_string(), "bug".to_string()];
    let text = Publication::Issue {
        kind: IssueKind::Feature,
        title: "Add a thing",
        labels: &labels,
        body: "Why it matters.",
    }
    .render("owner/repo")
    .expect("renders");
    assert_eq!(
        text,
        "feature issue on owner/repo\n\nTitle: Add a thing\n\nLabels: area: harness, bug\n\nWhy it matters."
    );
    let comment = COMMENT.render("owner/repo").expect("renders");
    assert_eq!(comment, "comment #7 on owner/repo\n\nbody");
}

#[tokio::test]
async fn a_bug_or_performance_issue_is_not_asked_about() {
    // A prompt would wait for the 30 s deadline.
    let gate = gate_with(IssueApprovalConfig::default(), Duration::from_secs(30));
    for kind in [IssueKind::Bug, IssueKind::Performance] {
        tokio::time::timeout(SHORT, gate.decide(issue(kind), false, "o/r"))
            .await
            .expect("no prompt was raised")
            .expect("approved");
    }
}

#[tokio::test]
async fn owned_pull_requests_and_comments_are_approved_without_a_prompt() {
    let gate = gate_with(IssueApprovalConfig::default(), Duration::from_secs(30));
    for publication in [PULL_REQUEST, COMMENT] {
        tokio::time::timeout(SHORT, gate.decide(publication, false, "o/r"))
            .await
            .expect("no prompt was raised")
            .expect("approved");
    }
}

#[tokio::test]
async fn an_unanswered_feature_issue_is_approved() {
    let gate = gate_with(IssueApprovalConfig::default(), SHORT);
    gate.decide(issue(IssueKind::Feature), false, "o/r")
        .await
        .expect("the auto entry approves when nobody answers");
}

#[tokio::test]
async fn an_unanswered_external_publication_is_denied() {
    let gate = gate_with(IssueApprovalConfig::default(), SHORT);
    for publication in [
        issue(IssueKind::Bug),
        issue(IssueKind::Feature),
        PULL_REQUEST,
        COMMENT,
    ] {
        let denied = gate
            .decide(publication, true, "o/r")
            .await
            .expect_err("a required entry never approves on timeout");
        assert!(denied.to_string().contains("denied"), "{denied}");
    }
}

#[tokio::test]
async fn an_answer_decides_a_required_prompt() {
    for approve in [true, false] {
        let gate = gate_with(IssueApprovalConfig::default(), Duration::from_secs(30));
        let responder = gate.approver.clone();
        let wait = tokio::spawn(async move { gate.decide(COMMENT, true, "o/r").await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        let pending = responder.pending_request_ids();
        assert_eq!(pending.len(), 1);
        assert!(responder.respond(&pending[0], approve));
        assert_eq!(wait.await.expect("join").is_ok(), approve);
    }
}

#[tokio::test]
async fn a_changed_setting_applies_to_the_next_publication() {
    let config = Arc::new(ArcSwap::from_pointee(MayConfig::default()));
    let gate = PublishGate::new(
        config.clone(),
        PermissionApprover::new()
            .with_approval_timeout(SHORT)
            .for_session("session-1"),
    );
    gate.decide(issue(IssueKind::Bug), false, "o/r")
        .await
        .expect("bug issues are not asked about by default");

    let mut changed = MayConfig::default();
    changed.issue_approval.bug = ApprovalPolicy::Required;
    config.store(Arc::new(changed));
    gate.decide(issue(IssueKind::Bug), false, "o/r")
        .await
        .expect_err("the saved setting now holds bug issues for approval");
}

#[tokio::test]
async fn a_session_that_cannot_ask_denies_required_and_approves_auto() {
    let gate = PublishGate::new(
        Arc::new(ArcSwap::from_pointee(MayConfig::default())),
        PermissionApprover::new()
            .for_session("session-1")
            .without_interactive_approval("a one-shot run"),
    );
    gate.decide(issue(IssueKind::Feature), false, "o/r")
        .await
        .expect("auto approves where nobody can answer");
    gate.decide(PULL_REQUEST, true, "o/r")
        .await
        .expect_err("required cannot be approved by nobody");
}

#[test]
fn the_defaults_parse_from_an_empty_section() {
    let config: MayConfig = toml::from_str("[issue_approval]\nbug = \"required\"\n").expect("toml");
    assert_eq!(config.issue_approval.bug, ApprovalPolicy::Required);
    assert_eq!(config.issue_approval.feature, ApprovalPolicy::Auto);
    assert_eq!(
        config.issue_approval.external_comment,
        ApprovalPolicy::Required
    );
}
