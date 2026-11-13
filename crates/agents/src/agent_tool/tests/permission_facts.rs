use super::*;
use base::interface::permission::{PermissionFacts, Reach, Subject};
use base::tool::Tool as _;
use serde_json::json;

#[tokio::test]
async fn the_agent_tool_delegates() {
    let tool = agent_tool_with_config(EngineConfig::defaults_for("test"));
    let input = json!({ "description": "d", "prompt": "p" });
    let ctx = platform::tool_context_for_test(std::path::PathBuf::from("/tmp"));
    assert_eq!(
        tool.permission_facts(&input, &ctx).await,
        PermissionFacts::new(Subject::None, Reach::Delegates)
    );
}
