use tempfile::tempdir;

use aither_agent::ExternalContent;
use aither_core::llm::model::Parameters;

use super::{
    AgentEnvironment, RuntimeSystemContext, build_agents_md_blocks, build_capabilities,
    build_efficiency, build_environment, build_work, tag,
};

#[test]
fn the_system_layout_holds_the_languages_slot() {
    assert!(tag::layout().instructions().contains(&tag::LANGUAGES));
}

fn test_context() -> aither_agent::Context {
    aither_agent::Context::with_layout(tag::layout())
}

fn test_memory_file() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempdir().expect("temp dir should be created");
    let path = dir.path().join("MEMORY.md");
    (dir, path)
}

fn memory(content: &str) -> ExternalContent {
    ExternalContent::data(super::MEMORY_SOURCE, content)
}

/// Durable system alerts pushed with `Context::push_alert`.
///
/// Alerts enter the conversation stream as `system` messages rather than
/// as ephemeral reminders, which are dropped by every request's reminder
/// repopulation pass.
fn system_alerts(context: &aither_agent::Context) -> Vec<String> {
    context
        .conversation_messages()
        .into_iter()
        .filter(|message| message.role() == aither_core::llm::Role::System)
        .map(|message| message.content().to_string())
        .collect()
}

#[test]
fn runtime_system_context_reinstates_memory_and_prunes_absent_blocks() {
    let (_dir, memory_path) = test_memory_file();
    let runtime_context = RuntimeSystemContext::new(memory_path, "remember this".to_string());
    let mut context = test_context();
    context
        .instructions_mut()
        .insert(&tag::PROFILE_PERSONA, "stale persona");
    context
        .instructions_mut()
        .insert(&tag::REPO_CLONES, "stale clones");
    context.external_mut().insert(
        &tag::WORKSPACE_WORKDIRS,
        ExternalContent::data("x", "stale"),
    );

    runtime_context.reconcile(&mut context);

    assert_eq!(
        context.external().get(&tag::MEMORY),
        Some(&memory("remember this"))
    );
    assert!(!context.instructions().contains(&tag::PROFILE_PERSONA));
    assert!(!context.instructions().contains(&tag::REPO_CLONES));
    assert!(!context.external().contains(&tag::WORKSPACE_WORKDIRS));
}

/// Before any request carried the prefix, nothing cached the old memory: a
/// change rewrites the block with no alert. The warm path, a diff while the
/// prefix stays as sent, is covered against a stub model in
/// `live_content_tests`.
#[test]
fn a_memory_change_before_any_request_rewrites_the_block_silently() {
    let (_dir, memory_path) = test_memory_file();
    std::fs::write(&memory_path, "before").expect("write initial memory");

    let mut runtime_context = RuntimeSystemContext::new(memory_path.clone(), "before".to_string());
    let mut context = test_context();
    runtime_context.reconcile(&mut context);

    std::fs::write(&memory_path, "after").expect("update memory");
    tokio_test::block_on(runtime_context.sync_memory_content(&mut context));
    runtime_context.reconcile(&mut context);

    assert_eq!(context.external().get(&tag::MEMORY), Some(&memory("after")));
    assert!(!context.has_staged_system_updates());
    assert_eq!(system_alerts(&context), [] as [String; 0]);
}

#[test]
fn instruction_blocks_reconcile_as_rendered() {
    let (_dir, memory_path) = test_memory_file();
    let identity = super::build_identity(false);
    let runtime_context = RuntimeSystemContext::new(memory_path, String::new())
        .with_instruction(tag::IDENTITY, identity.clone());
    let mut context = test_context();
    runtime_context.reconcile(&mut context);

    assert_eq!(context.instructions().get(&tag::IDENTITY), Some(&identity));
    assert!(identity.starts_with("<identity>\n"), "{identity}");
    assert!(identity.ends_with("</identity>"), "{identity}");
}

#[test]
#[should_panic(expected = "not an instruction block May renders at build")]
fn project_rules_are_not_set_as_a_fixed_block() {
    let (_dir, memory_path) = test_memory_file();
    let _ = RuntimeSystemContext::new(memory_path, String::new())
        .with_instruction(tag::PROJECT_RULES, String::new());
}

#[test]
fn workdirs_are_reconciled_independently() {
    let (_dir, memory_path) = test_memory_file();
    let mut runtime_context = RuntimeSystemContext::new(memory_path, String::new());
    let mut context = test_context();
    runtime_context.reconcile(&mut context);
    let without_workdirs_fingerprint = context.stable_prefix_fingerprint();

    runtime_context =
        runtime_context.with_workdirs(Some(ExternalContent::data("workdirs", "- /foo")));
    runtime_context.reconcile(&mut context);

    assert!(context.external().contains(&tag::WORKSPACE_WORKDIRS));
    assert_ne!(
        without_workdirs_fingerprint,
        context.stable_prefix_fingerprint(),
        "adding workdirs block must change the fingerprint",
    );
}

fn index(root: Option<&str>, nested: &[&str]) -> crate::agent::agents_md::AgentsIndex {
    crate::agent::agents_md::AgentsIndex {
        root_document: root.map(str::to_string),
        root_truncated: false,
        nested: nested.iter().map(|path| (*path).to_string()).collect(),
        extra_nested: 0,
    }
}

#[test]
fn agents_md_blocks_are_reconciled_and_pruned() {
    let (_dir, memory_path) = test_memory_file();
    let mut runtime_context = RuntimeSystemContext::new(memory_path, String::new());
    let mut context = test_context();
    runtime_context.reconcile(&mut context);
    assert!(!context.instructions().contains(&tag::PROJECT_RULES));
    assert!(!context.external().contains(&tag::AGENTS_MD));

    runtime_context = runtime_context.with_agents_md(build_agents_md_blocks(
        None,
        &index(Some("project rules"), &[]),
        "/launch/AGENTS.md",
        &[],
    ));
    runtime_context.reconcile(&mut context);
    assert_eq!(
        context.external().get(&tag::AGENTS_MD),
        Some(&ExternalContent::repository_instructions(
            "/launch/AGENTS.md",
            "project rules"
        ))
    );
    assert!(context.instructions().contains(&tag::PROJECT_RULES));

    runtime_context = runtime_context.with_agents_md(super::AgentsMdBlocks::default());
    runtime_context.reconcile(&mut context);
    assert!(!context.instructions().contains(&tag::PROJECT_RULES));
    assert!(!context.external().contains(&tag::AGENTS_MD));
}

#[test]
fn agents_md_blocks_list_nested_files_and_note_a_cut_root() {
    let mut scanned = index(Some("do the thing"), &["crates/foo/AGENTS.md"]);
    scanned.extra_nested = 2;
    scanned.root_truncated = true;
    let blocks = build_agents_md_blocks(None, &scanned, "/launch/AGENTS.md", &[]);
    let rules = blocks.rules.expect("rules render");
    assert!(
        rules.contains("read /launch/AGENTS.md for the rest"),
        "{rules}"
    );
    assert!(rules.contains("- crates/foo/AGENTS.md"), "{rules}");
    assert!(rules.contains("2 more"), "{rules}");
    assert!(
        blocks
            .root
            .expect("the root renders")
            .is_repository_instructions()
    );

    let empty = crate::agent::agents_md::AgentsIndex::default();
    assert_eq!(
        build_agents_md_blocks(None, &empty, "/launch/AGENTS.md", &[]),
        super::AgentsMdBlocks::default()
    );
}

#[test]
fn global_rules_render_alone_and_note_a_cut() {
    let global = crate::agent::agents_md::RulesDocument {
        content: "Answer in English.".to_string(),
        truncated: true,
    };
    let empty = crate::agent::agents_md::AgentsIndex::default();
    let blocks = build_agents_md_blocks(Some(&global), &empty, "/launch/AGENTS.md", &[]);

    assert_eq!(
        blocks.global,
        Some(ExternalContent::repository_instructions(
            "~/.may/AGENTS.md",
            "Answer in English."
        ))
    );
    assert_eq!(blocks.root, None);
    let rules = blocks.rules.expect("rules render");
    assert!(
        rules.contains("The global rules below are cut to fit the context."),
        "{rules}"
    );
    assert!(
        !rules.contains("launch directory's AGENTS.md below"),
        "{rules}"
    );
}

#[test]
fn global_rules_are_reread_each_sync() {
    let (dir, memory_path) = test_memory_file();
    let global_path = dir.path().join("AGENTS.md");
    let mut runtime_context = RuntimeSystemContext::new(memory_path, String::new())
        .with_global_agents_source(global_path.clone());
    let mut context = test_context();

    tokio_test::block_on(runtime_context.sync_agents_md(None));
    runtime_context.reconcile(&mut context);
    assert!(!context.external().contains(&tag::GLOBAL_AGENTS_MD));

    std::fs::write(&global_path, "Never push to main.\n").expect("write global rules");
    tokio_test::block_on(runtime_context.sync_agents_md(None));
    runtime_context.reconcile(&mut context);
    assert_eq!(
        context.external().get(&tag::GLOBAL_AGENTS_MD),
        Some(&ExternalContent::repository_instructions(
            "~/.may/AGENTS.md",
            "Never push to main.\n"
        ))
    );
    assert!(context.instructions().contains(&tag::PROJECT_RULES));
}

fn render_capabilities(commands: &[(String, String)], parameters: &Parameters) -> String {
    let dir = tempdir().expect("temp dir should be created");
    tokio_test::block_on(build_capabilities(commands, &[], parameters, dir.path()))
        .expect("capabilities render")
}

#[test]
fn the_computer_skill_is_listed_only_where_computer_is_registered() {
    let dir = tempdir().expect("temp dir should be created");
    let skill_dir = dir.path().join("computer");
    std::fs::create_dir_all(&skill_dir).expect("skill dir");
    std::fs::write(
        skill_dir.join("SKILL.md"),
        include_str!("../../../skills/computer/SKILL.md"),
    )
    .expect("write skill");
    let render = |commands: &[(String, String)]| {
        tokio_test::block_on(build_capabilities(
            commands,
            &[],
            &Parameters::default(),
            dir.path(),
        ))
        .expect("capabilities render")
    };

    let with = render(&[("computer".to_string(), "the desktop".to_string())]);
    assert!(with.contains("`skills/computer/SKILL.md`"), "{with}");
    let without = render(&[("browser".to_string(), "pages".to_string())]);
    assert!(!without.contains("skills/computer/SKILL.md"), "{without}");
}

#[test]
fn the_pr_landing_rule_is_rendered_only_for_a_session_with_a_clone() {
    let issues = crate::issue::IssueBackendInfo {
        name: "may",
        repository: None,
    };
    let with_clone = build_work(&issues, true, true, false);
    assert!(
        with_clone.contains("Land code for any repository with the `pr` command"),
        "{with_clone}"
    );
    let direct = build_work(&issues, false, true, false);
    assert!(!direct.contains("Land code for any repository"), "{direct}");
    assert!(!direct.contains("source checkout"), "{direct}");
}

#[test]
fn the_work_section_names_the_issue_backend() {
    let work = build_work(
        &crate::issue::IssueBackendInfo {
            name: "may",
            repository: None,
        },
        false,
        true,
        false,
    );
    assert!(work.contains("`issue` command (may backend)"), "{work}");

    let github = crate::issue::IssueBackendInfo {
        name: "github",
        repository: Some("May-Agent/may".to_string()),
    };
    let work = build_work(&github, false, true, false);
    assert!(work.contains("`issue` command (github backend)"), "{work}");
    assert!(!work.contains("May-Agent/may"), "{work}");

    let dir = tempdir().expect("temp dir should be created");
    let session = super::build_session(
        &AgentEnvironment::Local,
        &session_paths(&dir),
        None,
        None,
        Some("gitlab.com/group/sub/repo"),
        &github,
    );
    assert!(
        session.contains("Issue repository: May-Agent/may."),
        "{session}"
    );
    assert!(
        session.contains("Remote repository: gitlab.com/group/sub/repo."),
        "{session}"
    );
}

#[test]
fn capabilities_advertise_hosted_web_search_and_fetch() {
    let commands = vec![("browser".to_string(), "Drive the browser".to_string())];
    let mut parameters = Parameters::default();
    parameters.native_tools.claude.web_search = true;
    parameters.native_tools.claude.web_fetch = true;

    let prompt = render_capabilities(&commands, &parameters);

    assert!(prompt.contains("The `web_search` tool looks up current information"));
    assert!(prompt.contains("The `web_fetch` tool reads a known URL"));
    assert!(prompt.contains(
        "The `browser` command interacts with pages and reaches what search and fetch cannot"
    ));
}

/// The command rows are aither's live command table; capabilities only say
/// how commands run.
#[test]
fn capabilities_leave_the_command_rows_to_the_command_table() {
    let commands = vec![
        ("browser".to_string(), "Drive the browser".to_string()),
        ("webfetch".to_string(), "Fetch a URL".to_string()),
    ];

    let prompt = render_capabilities(&commands, &Parameters::default());

    assert!(!prompt.contains("web_search"), "{prompt}");
    assert!(!prompt.contains("- webfetch:"), "{prompt}");
    assert!(!prompt.contains("<commands>\n"), "{prompt}");
    assert!(
        prompt.contains("The commands in `<commands>` run as CLI commands inside `terminal`"),
        "{prompt}"
    );
}

#[test]
fn the_system_layout_places_the_command_table_after_capabilities() {
    let layout = tag::layout();
    let slots = layout.instructions();
    let capabilities = slots
        .iter()
        .position(|slot| *slot == tag::CAPABILITIES)
        .expect("capabilities are laid out");
    assert_eq!(slots.get(capabilities + 1), Some(&tag::COMMANDS));
}

#[test]
fn capabilities_list_subagents_in_their_current_form() {
    let prompt = render_capabilities(&[], &Parameters::default());
    assert!(
        prompt.contains("`subagent <name> --prompt-file <path>`"),
        "{prompt}"
    );
    assert!(prompt.contains("- explore: "), "{prompt}");
    assert!(prompt.contains("- fork: "), "{prompt}");
    assert!(!prompt.contains("--subagent"), "{prompt}");
    assert!(!prompt.contains("/subagents "), "{prompt}");
}

fn session_paths(dir: &tempfile::TempDir) -> crate::session::SessionPaths {
    crate::session::SessionPaths::new(dir.path().to_path_buf())
}

#[test]
fn remote_hosts_render_only_while_one_is_connected() {
    let alone = build_environment("model", &AgentEnvironment::Local, None, Vec::new());
    assert!(!alone.contains("ssh"), "{alone}");
    assert!(!alone.contains("Remote hosts"), "{alone}");

    let connected = build_environment(
        "model",
        &AgentEnvironment::Local,
        None,
        vec![crate::config::SshServerConfig {
            name: "build-box".to_string(),
            target: "me@build-box".to_string(),
            platform: crate::config::SshPlatformConfig {
                os: "Linux".to_string(),
                arch: "x86_64".to_string(),
            },
        }],
    );
    assert!(
        connected.contains("Remote hosts are connected"),
        "{connected}"
    );
    assert!(
        connected.contains("- build-box (me@build-box), Linux x86_64"),
        "{connected}"
    );
}

#[test]
fn the_environment_names_paths_relative_to_the_session_and_the_real_home() {
    let environment = build_environment("model", &AgentEnvironment::Local, None, Vec::new());
    assert!(environment.contains("\n- tmp/: "), "{environment}");
    assert!(!environment.contains("- /tmp"), "{environment}");
    let home = crate::config::MayConfig::home_dir().expect("home should resolve");
    assert!(
        environment.contains(&format!("the user's home is {}", home.display())),
        "{environment}"
    );
    let terminal = super::build_terminal(&AgentEnvironment::Local, false);
    assert!(
        terminal.contains("means `outputs/…` in the session directory given in `<session>`"),
        "{terminal}"
    );
    assert!(
        terminal.contains("- Use `uv` for Python dependencies (`uv add`, `uv pip install`)"),
        "{terminal}"
    );
    assert_eq!(
        terminal.contains("Prefer `rg`, `python3` or `jq`, and `fd` over `grep`, `awk` and `find`"),
        cfg!(not(windows)),
        "only the managed bash prefers them: {terminal}"
    );
}

fn container_environment() -> AgentEnvironment {
    use std::sync::Arc;

    use aither_sandbox::{AnyContainerExec, BollardContainerExec, HostRoute};

    // An HTTP client handle neither probes a socket nor dials until used;
    // rendering never uses it.
    let client =
        bollard::Docker::connect_with_http("http://127.0.0.1:1", 1, bollard::API_DEFAULT_VERSION)
            .expect("an HTTP Docker client handle is built without connecting");
    AgentEnvironment::Container {
        container_id: "test-container".to_string(),
        container_ipc_route: HostRoute::new(
            "host.docker.internal",
            std::net::IpAddr::from([127, 0, 0, 1]),
        )
        .expect("loopback is a usable host route"),
        container_exec: AnyContainerExec::new(Arc::new(BollardContainerExec::new(Arc::new(
            client,
        )))),
        browser_cdp_endpoint: None,
    }
}

fn clone_entry(session_paths: &crate::session::SessionPaths) -> super::RepoCloneEntry {
    super::RepoCloneEntry {
        clone_path: session_paths.root().join("repos/may"),
        source: "/Users/someone/may".to_string(),
    }
}

#[test]
fn container_repo_clones_render_under_the_workspace_mount() {
    let dir = tempdir().expect("temp dir should be created");
    let session_paths = session_paths(&dir);
    let rendered = super::build_repo_clones_context(
        &[clone_entry(&session_paths)],
        &container_environment(),
        &session_paths,
    )
    .expect("a clone renders a block");
    let host_prefix = dir.path().display().to_string();
    assert!(
        rendered.contains("- /workspace/repos/may (clone of"),
        "{rendered}"
    );
    assert!(!rendered.contains(&host_prefix), "{rendered}");
}

#[test]
fn native_repo_clones_keep_the_host_path() {
    let dir = tempdir().expect("temp dir should be created");
    let session_paths = session_paths(&dir);
    let rendered = super::build_repo_clones_context(
        &[clone_entry(&session_paths)],
        &AgentEnvironment::Local,
        &session_paths,
    )
    .expect("a clone renders a block");
    let host_path = dir.path().join("repos/may").display().to_string();
    assert!(
        rendered.contains(&format!("- {host_path} (clone of")),
        "{rendered}"
    );
}

#[test]
fn the_wake_alert_says_how_the_sleep_ended() {
    use crate::sleep::WakeCause;
    use askama::Template;

    let off = super::WakeAlert {
        cause: WakeCause::SleepOff,
    }
    .render()
    .expect("wake alert renders");
    assert!(off.contains("they ran /sleep off"), "{off}");
    let wrote = super::WakeAlert {
        cause: WakeCause::UserWrote,
    }
    .render()
    .expect("wake alert renders");
    assert!(wrote.contains("they came back and wrote"), "{wrote}");
}

#[test]
#[should_panic(expected = "outside the session directory")]
fn container_clone_outside_the_session_directory_panics() {
    let dir = tempdir().expect("temp dir should be created");
    let session_paths = session_paths(&dir);
    let stray = super::RepoCloneEntry {
        clone_path: std::path::PathBuf::from("/elsewhere/may"),
        source: "x".to_string(),
    };
    let _ = super::build_repo_clones_context(&[stray], &container_environment(), &session_paths);
}

#[test]
fn the_efficiency_section_prefers_automated_fixes_to_hand_edits() {
    let prompt = build_efficiency(&[], false);
    assert!(
        prompt.contains("Fix with automated tools before hand edits"),
        "{prompt}"
    );
    assert!(prompt.contains("`cargo clippy --fix`"), "{prompt}");
    assert!(
        prompt.contains(
            "Route lint findings with no autofix, and large batches of small manual fixes, to a `subagent general --tier fast`"
        ),
        "{prompt}"
    );
}

#[test]
fn the_efficiency_section_says_when_to_hand_exploration_to_explore() {
    let prompt = build_efficiency(&[], false);
    assert!(
        prompt.contains(
            "When answering means reading across many files and only the conclusion matters"
        ),
        "{prompt}"
    );
    assert!(
        prompt.contains("`explore --prompt-file <path>`"),
        "{prompt}"
    );
}

#[test]
fn the_capabilities_section_carries_each_core_commands_usage() {
    let core = [
        crate::terminal_commands::CoreCommandUsage {
            name: "ask_user".to_string(),
            usage: "Ask the user\n\nUsage:\n  ask_user <question>\n".to_string(),
        },
        crate::terminal_commands::CoreCommandUsage {
            name: "tasks".to_string(),
            usage: "Usage: tasks <subcommand>\n".to_string(),
        },
    ];
    let dir = tempdir().expect("temp dir should be created");
    let prompt = tokio_test::block_on(build_capabilities(
        &[],
        &core,
        &Parameters::default(),
        dir.path(),
    ))
    .expect("capabilities render");
    for command in &core {
        assert!(
            prompt.contains(&format!(
                "<usage command=\"{}\">\n{}\n</usage>",
                command.name,
                command.usage.trim()
            )),
            "{prompt}"
        );
    }
}

#[test]
fn a_blocked_step_runs_in_unsafe_mode_without_asking_first() {
    let prompt = super::build_permissions(&AgentEnvironment::Local, false, Vec::new(), None);
    assert!(
        prompt
            .contains("When the sandbox blocks a step of the work, run that step in `unsafe` mode"),
        "{prompt}"
    );
    assert!(
        prompt.contains("never ask through `ask_user` whether you may use `unsafe`"),
        "{prompt}"
    );
    let harness = super::build_harness();
    assert!(
        harness.contains("Get past it as the permissions section says"),
        "{harness}"
    );
}

#[test]
fn non_interactive_and_memory_off_omit_unsafe_approval_and_memory_instructions() {
    let permissions = super::build_permissions(&AgentEnvironment::Local, true, Vec::new(), None);
    assert!(permissions.contains("non-interactive"), "{permissions}");
    assert!(
        permissions.contains("`unsafe` mode is unavailable"),
        "{permissions}"
    );
    assert!(
        !permissions.contains("run that step in `unsafe` mode"),
        "{permissions}"
    );

    let identity = super::build_identity(true);
    assert!(identity.contains("non-interactive mode"), "{identity}");
    assert!(
        !identity.contains("Every question to the user goes through the `ask_user` command"),
        "{identity}"
    );

    let issues = crate::issue::IssueBackendInfo {
        name: "may",
        repository: None,
    };
    let work = build_work(&issues, true, false, true);
    assert!(!work.contains("MEMORY.md"), "{work}");
    assert!(!work.contains("~/.may/memories/"), "{work}");
    assert!(!work.contains("request_workspace"), "{work}");
}

/// The instruction blocks of a session in prompt order, as the model reads
/// them.
fn rendered_instructions(
    dir: &tempfile::TempDir,
    launch_dir: &str,
    clone_source: &str,
) -> Vec<(aither_agent::InstructionSlot, String)> {
    let (_memory_dir, memory_path) = test_memory_file();
    let paths = session_paths(dir);
    let issues = crate::issue::IssueBackendInfo {
        name: "may",
        repository: None,
    };
    let clones = super::build_repo_clones_context(
        &[super::RepoCloneEntry {
            clone_path: paths.root().join("repos/may"),
            source: clone_source.to_string(),
        }],
        &AgentEnvironment::Local,
        &paths,
    )
    .expect("a clone renders a block");
    let runtime_context = RuntimeSystemContext::new(memory_path, String::new())
        .with_instruction(tag::IDENTITY, super::build_identity(false))
        .with_instruction(tag::HARNESS, super::build_harness())
        .with_instruction(tag::EVIDENCE, super::build_evidence())
        .with_instruction(
            tag::ENVIRONMENT,
            build_environment("model", &AgentEnvironment::Local, None, Vec::new()),
        )
        .with_instruction(
            tag::TERMINAL,
            super::build_terminal(&AgentEnvironment::Local, false),
        )
        .with_instruction(tag::EFFICIENCY, build_efficiency(&[], false))
        .with_instruction(tag::WORK, build_work(&issues, true, true, false))
        .with_instruction(
            tag::SESSION,
            super::build_session(
                &AgentEnvironment::Local,
                &paths,
                Some(launch_dir.to_string()),
                None,
                None,
                &issues,
            ),
        )
        .with_instruction(tag::REPO_CLONES, clones);
    let mut context = test_context();
    runtime_context.reconcile(&mut context);
    tag::layout()
        .instructions()
        .iter()
        .filter_map(|slot| {
            context
                .instructions()
                .get(slot)
                .map(|text| (slot.clone(), text.clone()))
        })
        .collect()
}

#[test]
fn sessions_differing_only_in_session_values_share_the_prompt_up_to_the_session_tier() {
    let first_dir = tempdir().expect("temp dir should be created");
    let second_dir = tempdir().expect("temp dir should be created");
    let first = rendered_instructions(&first_dir, "/work/alpha", "/work/alpha");
    let second = rendered_instructions(&second_dir, "/work/beta", "/work/beta");

    let session_tier = [
        tag::SESSION,
        tag::REPO_CLONES,
        tag::PLAN_MODE,
        tag::PROJECT_RULES,
    ];
    let (shared_first, rest_first): (Vec<_>, Vec<_>) = first
        .iter()
        .partition(|(slot, _)| !session_tier.contains(slot));
    let (shared_second, rest_second): (Vec<_>, Vec<_>) = second
        .iter()
        .partition(|(slot, _)| !session_tier.contains(slot));
    assert!(
        shared_first.len() > 4,
        "the shared tiers hold the build-wide and configuration sections"
    );
    assert_eq!(shared_first, shared_second);
    assert_ne!(rest_first, rest_second);

    // No session-tier section comes before a shared one in the layout.
    let order: Vec<_> = first.iter().map(|(slot, _)| slot.clone()).collect();
    let first_session = order
        .iter()
        .position(|slot| session_tier.contains(slot))
        .expect("the session tier is rendered");
    assert!(
        order[first_session..]
            .iter()
            .all(|slot| session_tier.contains(slot))
    );
}

/// A subagent's instruction blocks carry the parent contract — `send_message`
/// mid-task, the last turn response as the report — and none of the
/// user-facing directives the main agent's sections hold.
#[test]
fn subagent_instructions_report_to_the_parent_and_never_the_user() {
    let blocks = super::build_subagent_instructions(
        &AgentEnvironment::Local,
        Vec::new(),
        false,
        vec!["API_KEY"],
        None,
        &Parameters::default(),
    );
    let text = blocks.join("\n");
    assert!(
        text.contains("`send_message`")
            && text.contains("your last response is delivered to your parent as your report"),
        "the parent contract is stated:\n{text}"
    );
    for user_facing in [
        "ask_user",
        "request_workspace",
        "the user's timezone",
        "probably in",
        "Each user message",
        "A user message that begins",
        "artifacts/",
        "files/",
        "<session>",
        "the session directory",
    ] {
        assert!(
            !text.contains(user_facing),
            "`{user_facing}` has no place in a subagent's prompt:\n{text}"
        );
    }
    // It still states the mechanisms it shares: the sandbox escalation is
    // its parent's decision, and secrets stay secret.
    assert!(
        text.contains("the agent that launched you approves")
            && text.contains("Stored secrets: API_KEY"),
        "{text}"
    );
}

#[test]
fn efficiency_routes_unautofixed_lint_findings_to_fast_subagent() {
    let efficiency = build_efficiency(&[], false);
    assert!(
        efficiency.contains("subagent general --tier fast"),
        "{efficiency}"
    );
    assert!(
        efficiency.contains("lint findings with no autofix"),
        "{efficiency}"
    );
}

#[test]
fn orchestrator_mode_renders_delta_without_duplicating_efficiency() {
    assert!(
        tag::layout()
            .instructions()
            .contains(&tag::ORCHESTRATOR_MODE)
    );
    assert_eq!(
        super::build_orchestrator_mode_context(&crate::session::ExecutionMode::Standard),
        None
    );

    let block =
        super::build_orchestrator_mode_context(&crate::session::ExecutionMode::Orchestrator)
            .expect("orchestrator mode renders");
    assert!(block.starts_with("<orchestrator_mode>\n"), "{block}");
    assert!(block.ends_with("</orchestrator_mode>"), "{block}");
    for expected in [
        "subagent fork",
        "subagent general --tier advanced",
        "subagent general --tier balanced",
        "subagent resume --id",
        "subagent-spaces/<run>/repo",
        "AGENTS.md",
        "repo clone",
        "foreground",
    ] {
        assert!(block.contains(expected), "missing `{expected}`:\n{block}");
    }
    // Only the delta over `<efficiency>` — never repeats `explore` or `--tier fast` lint routing.
    assert!(!block.contains("explore --prompt-file"), "{block}");
    assert!(!block.contains("--tier fast"), "{block}");
}

#[test]
fn orchestrator_mode_and_plan_mode_reconcile_orthogonally() {
    let (_dir, memory_path) = test_memory_file();
    let mut runtime_context = RuntimeSystemContext::new(memory_path, String::new());
    let mut context = test_context();

    runtime_context.set_instruction_opt(
        tag::PLAN_MODE,
        super::build_plan_mode_context(crate::session::SessionPlanState::Draft),
    );
    runtime_context.set_instruction_opt(
        tag::ORCHESTRATOR_MODE,
        super::build_orchestrator_mode_context(&crate::session::ExecutionMode::Orchestrator),
    );
    runtime_context.reconcile(&mut context);
    assert!(context.instructions().contains(&tag::PLAN_MODE));
    assert!(context.instructions().contains(&tag::ORCHESTRATOR_MODE));

    // Switching back to Standard removes `<orchestrator_mode>` without disturbing `<plan_mode>`.
    runtime_context.set_instruction_opt(
        tag::ORCHESTRATOR_MODE,
        super::build_orchestrator_mode_context(&crate::session::ExecutionMode::Standard),
    );
    runtime_context.reconcile(&mut context);
    assert!(context.instructions().contains(&tag::PLAN_MODE));
    assert!(!context.instructions().contains(&tag::ORCHESTRATOR_MODE));
}

#[test]
fn fusion_mode_templates_cover_lead_and_sidekick_contracts() {
    assert!(tag::layout().instructions().contains(&tag::FUSION_MODE));
    assert_eq!(
        super::build_fusion_lead_context(&crate::session::ExecutionMode::Standard),
        None
    );
    assert_eq!(
        super::build_fusion_lead_context(&crate::session::ExecutionMode::Orchestrator),
        None
    );

    let fusion = crate::session::ExecutionMode::Fusion {
        sidekick_model: crate::ModelSelector::new("openai", "gpt-5-mini"),
    };
    assert_eq!(super::build_orchestrator_mode_context(&fusion), None);

    let lead = super::build_fusion_lead_context(&fusion).expect("fusion lead renders");
    assert!(lead.starts_with("<fusion_mode>\n"), "{lead}");
    assert!(lead.ends_with("</fusion_mode>"), "{lead}");
    for expected in [
        "openai::gpt-5-mini",
        "sidekick",
        "queries",
        "metrics",
        "evaluation harnesses",
        "rubrics",
        "pipeline",
        "subagent",
    ] {
        assert!(lead.contains(expected), "missing `{expected}`:\n{lead}");
    }

    let sidekick = super::build_fusion_sidekick_context();
    assert!(sidekick.starts_with("<fusion_mode>\n"), "{sidekick}");
    assert!(sidekick.ends_with("</fusion_mode>"), "{sidekick}");
    for expected in ["sidekick", "lead", "shared repository clones", "background"] {
        assert!(
            sidekick.contains(expected),
            "missing `{expected}`:\n{sidekick}"
        );
    }
}

#[test]
fn fusion_mode_and_plan_mode_reconcile_orthogonally_without_touching_static_prefix() {
    let (_dir, memory_path) = test_memory_file();
    let mut runtime_context = RuntimeSystemContext::new(memory_path, String::new())
        .with_instruction(tag::IDENTITY, super::build_identity(false));
    let mut context = test_context();
    runtime_context.reconcile(&mut context);

    let fusion = crate::session::ExecutionMode::Fusion {
        sidekick_model: crate::ModelSelector::new("openai", "gpt-5-mini"),
    };
    runtime_context.set_instruction_opt(
        tag::PLAN_MODE,
        super::build_plan_mode_context(crate::session::SessionPlanState::Draft),
    );
    runtime_context
        .set_instruction_opt(tag::FUSION_MODE, super::build_fusion_lead_context(&fusion));
    runtime_context.reconcile(&mut context);
    assert!(context.instructions().contains(&tag::PLAN_MODE));
    assert!(context.instructions().contains(&tag::FUSION_MODE));
    assert!(context.instructions().contains(&tag::IDENTITY));

    runtime_context.set_instruction_opt(
        tag::FUSION_MODE,
        super::build_fusion_lead_context(&crate::session::ExecutionMode::Standard),
    );
    runtime_context.reconcile(&mut context);
    assert!(context.instructions().contains(&tag::PLAN_MODE));
    assert!(!context.instructions().contains(&tag::FUSION_MODE));
}

#[test]
fn cross_repo_guidance_routes_git_repositories_to_repo_clone_path() {
    let permissions = super::build_permissions(&AgentEnvironment::Local, false, Vec::new(), None);
    assert!(
        permissions.contains("repo clone --path <dir>"),
        "permissions must direct git repository edits to repo clone --path:\n{permissions}"
    );
    assert!(
        permissions.contains("non-repository directories"),
        "permissions must scope request_workspace to non-repo directories or explicit checkout edits:\n{permissions}"
    );

    let issues = crate::issue::IssueBackendInfo {
        name: "may",
        repository: None,
    };
    let work = build_work(&issues, true, true, false);
    assert!(
        work.contains("repo clone --path <dir>")
            && work.contains("run `pr` inside that workspace or pass `--repo`"),
        "work must direct cross-repository edits and PRs to repo clone --path and that workspace's pr:\n{work}"
    );

    let dir = tempdir().expect("temp dir should be created");
    let session = super::build_session(
        &AgentEnvironment::Local,
        &session_paths(&dir),
        Some("/tmp/may".to_string()),
        Some("/tmp/may".to_string()),
        None,
        &issues,
    );
    assert!(
        session.contains("repo clone --path <dir>"),
        "session must mention repo clone --path for another local repository:\n{session}"
    );
}
