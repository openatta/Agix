//! 方法表的元数据（名字、所属模块、是否编进了这个构建、params 与 result 的
//! schema），以及各方法的参数和结果类型。

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub struct MethodInfo {
    pub name: &'static str,
    /// The optional module the method belongs to; `None` for the core.
    pub module: Option<&'static str>,
    /// Whether that module is in this build. A method that is not answers
    /// `METHOD_NOT_FOUND` with `data.module`.
    pub compiled: bool,
    pub params: fn(&mut SchemaGenerator) -> Schema,
    pub result: fn(&mut SchemaGenerator) -> Schema,
}

pub fn schema_of<T: JsonSchema>(generator: &mut SchemaGenerator) -> Schema {
    generator.subschema_for::<T>()
}

/// Params or a result not typed yet: any JSON, passed through as is.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Untyped(pub Value);

impl JsonSchema for Untyped {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Untyped".into()
    }

    fn inline_schema() -> bool {
        true
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        schemars::json_schema!({
            "description": "未类型化:字段见 docs/daemon_rpc_protocol.md 对应小节"
        })
    }
}

/// `session.startAgent`'s params.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct StartAgentParams {
    pub session_id: String,
    pub prompt: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub subagent_type: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    /// Refused when given: a sub-agent runs on its parent's provider.
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    #[serde(default)]
    pub disallowed_tools: Vec<String>,
    #[serde(default)]
    pub permission_rules: Vec<base::permission::PermissionRule>,
    #[serde(default)]
    pub output_schema: Option<serde_json::Value>,
    #[serde(default = "notify_parent_by_default")]
    pub notify_parent: bool,
    #[serde(default)]
    pub tag: Option<String>,
}

fn notify_parent_by_default() -> bool {
    true
}

impl StartAgentParams {
    pub fn into_request(self) -> base::interface::agent_spawner::AgentRequest {
        base::interface::agent_spawner::AgentRequest {
            prompt: self.prompt,
            description: self.description,
            subagent_type: self.subagent_type,
            model: self.model,
            allowed_tools: self.allowed_tools,
            disallowed_tools: self.disallowed_tools,
            permission_rules: self.permission_rules,
            output_schema: self.output_schema,
            notify_parent: self.notify_parent,
            tag: self.tag,
        }
    }
}
