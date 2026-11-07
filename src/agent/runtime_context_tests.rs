use tempfile::tempdir;

use aither_agent::ExternalContent;
use aither_core::llm::model::Parameters;

use super::{
    AgentEnvironment, RuntimeSystemContext, build_agents_md_blocks, build_capabilities,
    build_environment, build_work, tag,
};

#[test]
fn the_system_layout_holds_the_languages_slot() {
    assert!(
        tag::layout()
            .instructions()
            .contains(&aither_agent::InstructionSlot::LANGUAGES)
    );
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
    let identity = super::build_identity();
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
    tokio_test::block_on(build_capabilities(commands, parameters, dir.path()))
        .expect("capabilities render")
}

#[test]
fn the_work_section_names_the_issue_backend() {
    let work = build_work(&crate::issue::IssueBackendInfo {
        name: "may",
        repository: None,
    });
    assert!(work.contains("`issue` command (may backend)"), "{work}");

    let work = build_work(&crate::issue::IssueBackendInfo {
        name: "github",
        repository: Some("May-Agent/may".to_string()),
    });
    assert!(
        work.contains("`issue` command (github backend: May-Agent/may)"),
        "{work}"
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
    assert_eq!(
        slots.get(capabilities + 1),
        Some(&aither_agent::InstructionSlot::COMMANDS)
    );
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
    let dir = tempdir().expect("temp dir should be created");
    let paths = session_paths(&dir);
    let alone = build_environment(
        "model",
        &AgentEnvironment::Local,
        &paths,
        None,
        Vec::new(),
        None,
    );
    assert!(!alone.contains("ssh"), "{alone}");
    assert!(!alone.contains("Remote hosts"), "{alone}");

    let connected = build_environment(
        "model",
        &AgentEnvironment::Local,
        &paths,
        None,
        vec![crate::config::SshServerConfig {
            name: "build-box".to_string(),
            target: "me@build-box".to_string(),
        }],
        None,
    );
    assert!(
        connected.contains("Remote hosts are connected"),
        "{connected}"
    );
    assert!(
        connected.contains("- build-box (me@build-box)"),
        "{connected}"
    );
}

#[test]
fn the_environment_names_paths_relative_to_the_session_and_the_real_home() {
    let dir = tempdir().expect("temp dir should be created");
    let paths = session_paths(&dir);
    let environment = build_environment(
        "model",
        &AgentEnvironment::Local,
        &paths,
        None,
        Vec::new(),
        None,
    );
    assert!(environment.contains("\n- tmp/: "), "{environment}");
    assert!(!environment.contains("- /tmp"), "{environment}");
    let home = crate::config::MayConfig::home_dir().expect("home should resolve");
    assert!(
        environment.contains(&format!("the user's home is {}", home.display())),
        "{environment}"
    );
    let terminal = super::build_terminal(&AgentEnvironment::Local, &paths);
    assert!(
        terminal.contains(&format!("means {}/outputs/", dir.path().display())),
        "{terminal}"
    );
}

fn container_environment() -> AgentEnvironment {
    use std::sync::Arc;

    use aither_sandbox::{
        AnyContainerExec, BollardContainerExec, BridgeAddress, ContainerIpcRoute,
    };

    // An HTTP client handle neither probes a socket nor dials until used;
    // rendering never uses it.
    let client =
        bollard::Docker::connect_with_http("http://127.0.0.1:1", 1, bollard::API_DEFAULT_VERSION)
            .expect("an HTTP Docker client handle is built without connecting");
    AgentEnvironment::Container {
        container_id: "test-container".to_string(),
        container_ipc_route: ContainerIpcRoute::new(
            "host.docker.internal",
            BridgeAddress::try_from(std::net::IpAddr::from([127, 0, 0, 1]))
                .expect("loopback binds the bridge"),
        ),
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
