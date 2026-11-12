use std::sync::Arc;

use aion_agent::bootstrap::AgentBootstrap;
use aion_agent::output::null_sink::NullSink;
use aion_config::compat::ProviderCompat;
use aion_config::config::{Config, ProviderType};
use aion_skills::frontmatter::{parse_frontmatter, parse_skill_fields};
use aion_skills::types::{LoadedFrom, SkillSource};
use serde_json::json;

#[tokio::test]
async fn host_supplied_skills_are_available_without_installing_global_skills() {
    let dir = tempfile::tempdir().unwrap();
    let parsed = parse_frontmatter("---\nname: officecli-xlsx\ndescription: Host skill\n---\nHOST_SKILL_MARKER");
    let skill = parse_skill_fields(
        &parsed.frontmatter,
        &parsed.content,
        "officecli-xlsx",
        SkillSource::Managed,
        LoadedFrom::Managed,
        Some(dir.path().to_str().unwrap()),
    );
    let mut result = AgentBootstrap::new(minimal_config(), dir.path().to_string_lossy(), null_output())
        .skills(vec![skill])
        .build()
        .await
        .unwrap();

    let tool = result.engine.registry_mut().get("Skill").unwrap();
    let loaded = tool.execute(json!({"skill":"officecli-xlsx"})).await;
    assert!(!loaded.is_error, "{}", loaded.content);
    assert!(loaded.content.contains("HOST_SKILL_MARKER"));
    let denied = tool.execute(json!({"skill":"unselected-global-skill"})).await;
    assert!(denied.is_error);
    assert!(denied.content.contains("not found"));
}

fn minimal_config() -> Config {
    Config {
        provider_label: "openai".into(),
        provider: ProviderType::OpenAI,
        api_key: "sk-test".into(),
        base_url: "http://localhost:0".into(),
        model: "gpt-test-model".into(),
        max_tokens: Some(1024),
        max_turns: Some(5),
        max_tool_call_malformed_turns: Some(3),
        max_tool_call_failure_turns: Some(3),
        system_prompt: None,
        thinking: None,
        prompt_caching: false,
        compat: ProviderCompat::openai_defaults(),
        tools: Default::default(),
        session: Default::default(),
        compact: Default::default(),
        plan: Default::default(),
        shell: Default::default(),
        file_cache: Default::default(),
        hooks: Default::default(),
        bedrock: None,
        vertex: None,
        mcp: Default::default(),
        logging: Default::default(),
    }
}

#[tokio::test]
async fn empty_host_skill_snapshot_does_not_discover_workspace_skills() {
    let dir = tempfile::tempdir().unwrap();
    let skill_dir = dir.path().join(".aionrs/skills/unselected");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(skill_dir.join("SKILL.md"), "UNSELECTED_MARKER").unwrap();
    let mut result = AgentBootstrap::new(minimal_config(), dir.path().to_string_lossy(), null_output())
        .skills(Vec::new())
        .build()
        .await
        .unwrap();
    let loaded = result
        .engine
        .registry_mut()
        .get("Skill")
        .unwrap()
        .execute(json!({"skill":"unselected"}))
        .await;
    assert!(loaded.is_error);
    assert!(!loaded.content.contains("UNSELECTED_MARKER"));
}

#[tokio::test]
async fn host_skill_snapshot_still_enforces_deny_rules() {
    let dir = tempfile::tempdir().unwrap();
    let parsed = parse_frontmatter("HOST_SKILL_MARKER");
    let skill = parse_skill_fields(
        &parsed.frontmatter,
        &parsed.content,
        "blocked",
        SkillSource::Managed,
        LoadedFrom::Managed,
        Some(dir.path().to_str().unwrap()),
    );
    let mut config = minimal_config();
    config.tools.auto_approve = true;
    config.tools.skills.deny = vec!["blocked".into()];
    let mut result = AgentBootstrap::new(config, dir.path().to_string_lossy(), null_output())
        .skills(vec![skill])
        .build()
        .await
        .unwrap();
    let loaded = result
        .engine
        .registry_mut()
        .get("Skill")
        .unwrap()
        .execute(json!({"skill":"blocked"}))
        .await;
    assert!(loaded.is_error);
    assert!(loaded.content.contains("denied"));
}

fn null_output() -> Arc<dyn aion_agent::output::OutputSink> {
    Arc::new(NullSink)
}

#[tokio::test]
async fn bootstrap_builds_engine_with_model_in_prompt() {
    let config = minimal_config();
    let result = AgentBootstrap::new(config, "/tmp/test-workspace", null_output())
        .build()
        .await
        .expect("bootstrap should succeed");

    assert!(!result.engine.tool_names().is_empty());
    assert!(!result.has_mcp);
    assert!(result.mcp_managers.is_empty());
}

#[tokio::test]
async fn bootstrap_registers_all_expected_tools() {
    let config = minimal_config();
    let result = AgentBootstrap::new(config, "/tmp/test-workspace", null_output())
        .build()
        .await
        .unwrap();

    let names = result.engine.tool_names();

    for expected in &["Read", "Write", "Edit", "ExecCommand", "Grep", "Glob"] {
        assert!(names.iter().any(|n| n == expected), "missing built-in tool: {expected}");
    }

    assert!(names.iter().any(|n| n == "Skill"), "SkillTool should be registered");
    assert!(names.iter().any(|n| n == "Spawn"), "SpawnTool should be registered");
    assert!(
        names.iter().any(|n| n == "ToolSearch"),
        "ToolSearchTool should be registered"
    );
}

#[tokio::test]
async fn bootstrap_plan_tools_when_enabled() {
    let mut config = minimal_config();
    config.plan.enabled = true;

    let result = AgentBootstrap::new(config, "/tmp/test-workspace", null_output())
        .build()
        .await
        .unwrap();

    let names = result.engine.tool_names();
    assert!(
        names.iter().any(|n| n == "EnterPlanMode"),
        "EnterPlanMode should be registered when plan.enabled"
    );
    assert!(
        names.iter().any(|n| n == "ExitPlanMode"),
        "ExitPlanMode should be registered when plan.enabled"
    );
}

#[tokio::test]
async fn bootstrap_no_plan_tools_when_disabled() {
    let mut config = minimal_config();
    config.plan.enabled = false;

    let result = AgentBootstrap::new(config, "/tmp/test-workspace", null_output())
        .build()
        .await
        .unwrap();

    let names = result.engine.tool_names();
    assert!(
        !names.iter().any(|n| n == "EnterPlanMode"),
        "EnterPlanMode should NOT be registered when plan.disabled"
    );
}

#[tokio::test]
async fn bootstrap_no_mcp_when_no_servers() {
    let config = minimal_config();
    let result = AgentBootstrap::new(config, "/tmp/test-workspace", null_output())
        .build()
        .await
        .unwrap();

    assert!(!result.has_mcp);
    assert!(result.mcp_managers.is_empty());
}

#[tokio::test]
async fn bootstrap_with_custom_system_prompt() {
    let mut config = minimal_config();
    config.system_prompt = Some("You are a pirate assistant.".into());

    let _result = AgentBootstrap::new(config, "/tmp/test-workspace", null_output())
        .build()
        .await
        .unwrap();
}

#[tokio::test]
async fn bootstrap_with_agents_md_in_workspace() {
    let tmp = tempfile::TempDir::new().unwrap();
    let workspace = tmp.path();
    std::fs::write(workspace.join("AGENTS.md"), "PROJECT_RULES_MARKER").unwrap();

    let config = minimal_config();
    let _result = AgentBootstrap::new(config, workspace.to_string_lossy().as_ref(), null_output())
        .build()
        .await
        .unwrap();
}

#[tokio::test]
async fn bootstrap_config_accessor_returns_config() {
    let config = minimal_config();
    let bootstrap = AgentBootstrap::new(config, "/tmp/ws", null_output());
    assert_eq!(bootstrap.config().model, "gpt-test-model");
    assert_eq!(bootstrap.config().max_tokens, Some(1024));
}

#[tokio::test]
async fn bootstrap_with_external_provider() {
    let config = minimal_config();
    let provider = aion_providers::create_provider(&config);

    let result = AgentBootstrap::new(config, "/tmp/test-workspace", null_output())
        .provider(provider)
        .build()
        .await
        .unwrap();

    assert!(!result.engine.tool_names().is_empty());
}
