use super::*;
use base::interface::permission::{PermissionFacts, Reach};
use base::tool::Tool as _;
use serde_json::json;

#[tokio::test]
async fn the_agent_tool_delegates_and_runs_as_before() {
    let tool = agent_tool_with_config(EngineConfig::defaults_for("test"));
    let input = json!({ "description": "d", "prompt": "p" });
    let ctx = platform::tool_context_for_test(std::path::PathBuf::from("/tmp"));
    let stated = tool.permission_facts(&input, &ctx).await;
    let old = PermissionFacts::bridged(&tool, &input, &ctx).await;
    assert_eq!(stated.reach, Reach::Delegates);
    assert_eq!(old.reach, Reach::SessionOnly);
    assert_eq!(stated.subject, old.subject);
    assert_eq!(stated.question, old.question);
    assert_eq!(stated.runs, old.runs);
}
