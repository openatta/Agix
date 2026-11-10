//! Scene-level deferred-tool policy.
//!
//! `ToolSearchTool` ("ToolSearch") already exists and is registered, but
//! nothing ever produced a *deferred* tool for it to find in a real session:
//! every registered tool's full JSON schema went into the request's `tools`
//! array on every API call, whether the scene expected it to be used or not.
//!
//! This module supplies the missing half — a scene declares tool names as
//! deferred (`Scene::deferred_tools()`), and those tools get wrapped in
//! [`DeferredTool`] before they reach the session registry. The wrapper is a
//! transparent delegate except for two things:
//!
//! - `input_schema()` collapses to an empty object schema, so the per-call
//!   cost of the entry drops to its name and description; a call that fails
//!   validation is answered with the real schema;
//! - `is_deferred()` reports `true`, which is exactly the predicate
//!   `ToolSearchTool` filters on — so the tool stays *discoverable by name*
//!   through `ToolSearch{query: "select:<name>"}`.
//!
//! Execution is unaffected: `call`/`validate_input`/`check_permissions` and
//! every behavior predicate delegate to the wrapped tool, so a deferred tool
//! the model decides to invoke behaves identically to a non-deferred one.
//! What holds for every call goes across as one `spec()`; only what is handed
//! the call's input is forwarded method by method.
//! This is deliberately a wrapper rather than a change to the tool registry
//! or to `build_tool_defs` — the registry stays a plain name → tool map.

use async_trait::async_trait;
use base::error::ToolError;
use base::tool::{
    InterruptBehavior, PermissionDecision, ProgressSender, PromptContext, Tool, ToolContext,
    ToolResult, ToolSpec, ValidationResult,
};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;

/// Wraps a tool so it is advertised by name only — see the module docs.
pub struct DeferredTool {
    inner: Arc<dyn Tool>,
}

impl DeferredTool {
    /// Wrap `inner`. Prefer [`apply_deferred_policy`], which applies a whole
    /// scene policy at once.
    pub fn new(inner: Arc<dyn Tool>) -> Self {
        Self { inner }
    }

    /// The wrapped tool.
    pub fn inner(&self) -> &Arc<dyn Tool> {
        &self.inner
    }
}

#[async_trait]
impl Tool for DeferredTool {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn description(&self) -> &str {
        self.inner.description()
    }

    /// The whole point: a stub in place of the real schema. Kept a valid
    /// (if empty) JSON Schema object so a provider that validates the `tools`
    /// array still accepts it. What an empty schema means is said once, in
    /// `ToolSearch`'s description, not in every stub.
    fn input_schema(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }

    fn full_input_schema(&self) -> Value {
        self.inner.full_input_schema()
    }

    fn spec(&self) -> ToolSpec {
        let inner = self.inner.spec();
        // `ToolSearch` renders this next to the name in its results; without
        // the fallback a deferred tool would be listed as a bare name.
        let short_description = inner
            .short_description
            .clone()
            .or_else(|| Some(inner.description.clone()))
            .filter(|s| !s.is_empty());
        ToolSpec {
            deferred: true,
            short_description,
            // Not the inner tool's: `build_tool_defs` and `sync_mcp_adapters`
            // read an `mcp:` source as the raw adapter, and would advertise the
            // full-schema adapter in place of this wrapper.
            source: "builtin".to_string(),
            ..inner
        }
    }

    async fn prompt(&self, ctx: &PromptContext) -> String {
        self.inner.prompt(ctx).await
    }

    fn prompt_fragment(&self) -> String {
        self.inner.prompt_fragment()
    }

    fn is_read_only(&self, input: &Value) -> bool {
        self.inner.is_read_only(input)
    }

    fn is_concurrency_safe(&self, input: &Value) -> bool {
        self.inner.is_concurrency_safe(input)
    }

    fn is_destructive(&self, input: &Value) -> bool {
        self.inner.is_destructive(input)
    }

    fn permission_match_content(&self, input: &Value) -> Option<String> {
        self.inner.permission_match_content(input)
    }

    fn accept_edits_allows(&self, input: &Value, ctx: &ToolContext) -> bool {
        self.inner.accept_edits_allows(input, ctx)
    }

    fn affected_paths(&self, input: &Value) -> Vec<PathBuf> {
        self.inner.affected_paths(input)
    }

    fn interrupt_behavior(&self, input: &Value) -> InterruptBehavior {
        self.inner.interrupt_behavior(input)
    }

    async fn validate_input(&self, input: &Value, ctx: &ToolContext) -> ValidationResult {
        match self.inner.validate_input(input, ctx).await {
            ValidationResult::Err(message, code) => {
                ValidationResult::Err(self.with_schema(&message), code)
            }
            ok => ok,
        }
    }

    async fn check_permissions(&self, input: &Value, ctx: &ToolContext) -> PermissionDecision {
        self.inner.check_permissions(input, ctx).await
    }

    async fn call(
        &self,
        input: Value,
        ctx: ToolContext,
        progress: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        match self.inner.call(input, ctx, progress).await {
            Err(ToolError::Schema(e)) => {
                Err(ToolError::InvalidInput(self.with_schema(&e.to_string())))
            }
            Err(ToolError::InvalidInput(m)) => Err(ToolError::InvalidInput(self.with_schema(&m))),
            Err(ToolError::Validation(m)) => Err(ToolError::Validation(self.with_schema(&m))),
            other => other,
        }
    }
}

impl DeferredTool {
    /// A model that called the tool without fetching its schema gets the
    /// schema with the refusal, so the retry can be right the first time.
    fn with_schema(&self, message: &str) -> String {
        format!(
            "{message}\n\nInput schema of {}: {}",
            self.inner.name(),
            base::tool::compact_schema(self.inner.full_input_schema())
        )
    }
}

/// Apply a scene's deferred-tool policy to a tool list.
///
/// Tools whose name appears in `deferred` are wrapped in [`DeferredTool`];
/// everything else passes through untouched. Names in `deferred` that match
/// nothing are ignored — a scene naming a tool this deployment doesn't
/// register is not an error, the same way a `tools()` whitelist entry with no
/// matching tool isn't.
///
/// `Tool::is_deferred()` is deliberately **not** consulted: it is what
/// `ToolSearchTool` filters on, but only the wrapper collapses
/// `input_schema()`. Skipping tools that already report `true` would leave
/// them advertised as deferred while still shipping their full schema.
///
/// Wrapping is idempotent in the way that matters — a wrapped tool's schema
/// is already the stub and every other method delegates — but nesting is
/// still pointless indirection, so `Builder::build()` calls this exactly
/// once, after every tool is registered.
pub fn apply_deferred_policy(tools: Vec<Arc<dyn Tool>>, deferred: &[String]) -> Vec<Arc<dyn Tool>> {
    if deferred.is_empty() {
        return tools;
    }
    tools
        .into_iter()
        .map(|t| {
            if deferred.iter().any(|n| n == t.name()) {
                Arc::new(DeferredTool::new(t)) as Arc<dyn Tool>
            } else {
                t
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::tool::{InMemoryToolRegistry, PermissionContentKind};

    /// Every `Tool` that wraps another, wrapping `inner`.
    fn wrappers(inner: Arc<dyn Tool>) -> Vec<(&'static str, Arc<dyn Tool>)> {
        vec![("DeferredTool", Arc::new(DeferredTool::new(inner)))]
    }

    /// What a wrapper deliberately says differently from the tool it wraps.
    fn as_wrapped(wrapper: &str, inner: ToolSpec) -> ToolSpec {
        match wrapper {
            "DeferredTool" => ToolSpec {
                source: "builtin".into(),
                ..inner
            },
            other => panic!("{other}: say what it changes, or that it changes nothing"),
        }
    }

    fn unusual_spec(tool: &dyn Tool) -> ToolSpec {
        ToolSpec {
            source: "mcp:elsewhere".into(),
            short_description: Some("short".into()),
            enabled: false,
            read_only: true,
            concurrency_safe: true,
            destructive: true,
            interrupt_behavior: InterruptBehavior::Block,
            strict: true,
            deferred: true,
            dynamic: true,
            direct: true,
            callable_from_code: true,
            code_result: Some("{n: number}"),
            needs_dispatch: true,
            permission_content_kind: PermissionContentKind::WritePath,
            self_allow_overrides_mode: true,
            leaves_plan_mode: true,
            ..ToolSpec::defaults_for(tool)
        }
    }

    struct Declared;
    #[async_trait]
    impl Tool for Declared {
        fn name(&self) -> &str {
            "Declared"
        }
        fn description(&self) -> &str {
            "declares a spec"
        }
        fn input_schema(&self) -> Value {
            json!({"type": "object"})
        }
        fn spec(&self) -> ToolSpec {
            unusual_spec(self)
        }
        fn is_read_only(&self, input: &Value) -> bool {
            input.get("peek").is_some()
        }
        async fn call(
            &self,
            _: Value,
            _: ToolContext,
            _: ProgressSender,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::text("ok"))
        }
    }

    struct Methods;
    #[async_trait]
    impl Tool for Methods {
        fn name(&self) -> &str {
            "Methods"
        }
        fn description(&self) -> &str {
            "overrides every method"
        }
        fn input_schema(&self) -> Value {
            json!({"type": "object"})
        }
        fn source(&self) -> std::borrow::Cow<'_, str> {
            "mcp:elsewhere".into()
        }
        fn short_description(&self) -> Option<String> {
            Some("short".into())
        }
        fn is_enabled(&self) -> bool {
            false
        }
        fn is_read_only(&self, _: &Value) -> bool {
            true
        }
        fn is_concurrency_safe(&self, _: &Value) -> bool {
            true
        }
        fn is_destructive(&self, _: &Value) -> bool {
            true
        }
        fn interrupt_behavior(&self, _: &Value) -> InterruptBehavior {
            InterruptBehavior::Block
        }
        fn strict(&self) -> bool {
            true
        }
        fn is_deferred(&self) -> bool {
            true
        }
        fn is_dynamic(&self) -> bool {
            true
        }
        fn is_direct(&self) -> bool {
            true
        }
        fn callable_from_code(&self) -> bool {
            true
        }
        fn code_result(&self) -> Option<&'static str> {
            Some("{n: number}")
        }
        fn needs_dispatch(&self) -> bool {
            true
        }
        fn permission_content_kind(&self) -> PermissionContentKind {
            PermissionContentKind::WritePath
        }
        fn self_allow_overrides_mode(&self) -> bool {
            true
        }
        fn leaves_plan_mode(&self) -> bool {
            true
        }
        async fn call(
            &self,
            _: Value,
            _: ToolContext,
            _: ProgressSender,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::text("ok"))
        }
    }

    #[test]
    fn a_wrapper_says_what_the_tool_it_wraps_says() {
        let inners: Vec<Arc<dyn Tool>> = vec![Arc::new(Declared), Arc::new(Methods)];
        for inner in inners {
            let expected_inner = unusual_spec(inner.as_ref());
            assert_eq!(inner.spec(), expected_inner, "{}", inner.name());
            for (wrapper, wrapped) in wrappers(inner.clone()) {
                let context = format!("{wrapper} around {}", inner.name());
                assert_eq!(
                    wrapped.spec(),
                    as_wrapped(wrapper, expected_inner.clone()),
                    "{context}"
                );
                assert_eq!(wrapped.is_enabled(), inner.is_enabled(), "{context}");
                assert_eq!(wrapped.strict(), inner.strict(), "{context}");
                assert_eq!(
                    wrapped.leaves_plan_mode(),
                    inner.leaves_plan_mode(),
                    "{context}"
                );
                for input in [json!({}), json!({"peek": true})] {
                    assert_eq!(
                        wrapped.is_read_only(&input),
                        inner.is_read_only(&input),
                        "{context}: {input}"
                    );
                    assert_eq!(
                        wrapped.is_concurrency_safe(&input),
                        inner.is_concurrency_safe(&input),
                        "{context}"
                    );
                    assert_eq!(
                        wrapped.is_destructive(&input),
                        inner.is_destructive(&input),
                        "{context}"
                    );
                    assert_eq!(
                        wrapped.interrupt_behavior(&input),
                        inner.interrupt_behavior(&input),
                        "{context}"
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn a_call_with_the_wrong_input_is_answered_with_the_schema() {
        struct Strict;
        #[async_trait]
        impl Tool for Strict {
            fn name(&self) -> &str {
                "Strict"
            }
            fn description(&self) -> &str {
                "needs a path"
            }
            fn input_schema(&self) -> Value {
                json!({"type": "object", "properties": {"path": {"type": "string"}},
                       "required": ["path"]})
            }
            async fn call(
                &self,
                input: Value,
                _: ToolContext,
                _: ProgressSender,
            ) -> Result<ToolResult, ToolError> {
                #[derive(serde::Deserialize)]
                #[allow(dead_code)]
                struct In {
                    path: String,
                }
                let _: In = serde_json::from_value(input)?;
                Ok(ToolResult::text("ok"))
            }
        }

        let deferred = DeferredTool::new(Arc::new(Strict));
        let err = deferred
            .call(
                json!({"file": "a.txt"}),
                ToolContext::for_test("/tmp".into()),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("missing field `path`"), "{err}");
        assert!(err.contains("Input schema of Strict"), "{err}");
        assert!(err.contains("\"required\":[\"path\"]"), "{err}");
    }

    fn tool_named(name: &'static str) -> Arc<dyn Tool> {
        struct T(&'static str);
        #[async_trait]
        impl Tool for T {
            fn name(&self) -> &str {
                self.0
            }
            fn description(&self) -> &str {
                "does a thing"
            }
            fn input_schema(&self) -> Value {
                json!({"type": "object", "properties": {"path": {"type": "string"}},
                       "required": ["path"]})
            }
            fn is_read_only(&self, _: &Value) -> bool {
                true
            }
            async fn call(
                &self,
                input: Value,
                _: ToolContext,
                _: ProgressSender,
            ) -> Result<ToolResult, ToolError> {
                Ok(ToolResult::text(format!("ran with {input}")))
            }
        }
        Arc::new(T(name))
    }

    #[test]
    fn a_deferred_tool_without_a_short_description_is_listed_with_its_description() {
        let deferred = DeferredTool::new(tool_named("Grep"));
        assert_eq!(
            deferred.short_description().as_deref(),
            Some("does a thing")
        );
        assert!(deferred.is_read_only(&Value::Null));
    }

    #[test]
    fn deferring_a_tool_keeps_what_the_permission_gate_asks_of_it() {
        let out = apply_deferred_policy(
            vec![
                Arc::new(crate::plan_mode::ExitPlanModeTool) as Arc<dyn Tool>,
                Arc::new(crate::task_message::TaskMessageTool),
            ],
            &["ExitPlanMode".to_string(), "TaskMessage".to_string()],
        );
        assert!(out.iter().all(|t| t.is_deferred()));
        let exit = out.iter().find(|t| t.name() == "ExitPlanMode").unwrap();
        assert!(exit.leaves_plan_mode());
        let message = out.iter().find(|t| t.name() == "TaskMessage").unwrap();
        assert!(message.self_allow_overrides_mode());
    }

    #[test]
    fn deferred_entry_drops_the_schema_but_keeps_the_name() {
        let out = apply_deferred_policy(
            vec![tool_named("Grep"), tool_named("Read")],
            &["Grep".to_string()],
        );
        let grep = out.iter().find(|t| t.name() == "Grep").unwrap();
        let read = out.iter().find(|t| t.name() == "Read").unwrap();

        // Name + description survive — that's what the model sees.
        assert_eq!(grep.name(), "Grep");
        assert_eq!(grep.description(), "does a thing");
        // The real schema does not.
        assert_eq!(
            grep.input_schema(),
            json!({"type": "object", "properties": {}}),
            "deferred tool must not carry its real schema"
        );
        // Untouched tool keeps everything.
        assert_eq!(read.input_schema()["required"], json!(["path"]));
        assert!(!read.is_deferred());
    }

    #[tokio::test]
    async fn deferred_tool_is_still_discoverable_and_callable() {
        let reg = Arc::new(InMemoryToolRegistry::new());
        for t in apply_deferred_policy(vec![tool_named("Grep")], &["Grep".to_string()]) {
            reg.register(t);
        }

        // Discoverable: ToolSearch only looks at `is_deferred()` tools.
        let search = crate::tool_search::ToolSearchTool::new(reg.clone());
        let found = search
            .call(
                json!({"query": "select:Grep"}),
                ToolContext::for_test("/tmp".into()),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        match &found.content {
            base::tool::ToolResultContent::Text(s) => assert!(s.contains("Grep"), "{s}"),
            _ => panic!("expected text"),
        }

        // Callable: dispatch still lands on the wrapped tool.
        let ran = reg
            .get("Grep")
            .unwrap()
            .call(
                json!({"path": "x"}),
                ToolContext::for_test("/tmp".into()),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        match &ran.content {
            base::tool::ToolResultContent::Text(s) => assert!(s.contains("ran with"), "{s}"),
            _ => panic!("expected text"),
        }
    }

    #[test]
    fn empty_policy_and_unknown_names_are_no_ops() {
        let out = apply_deferred_policy(vec![tool_named("Read")], &[]);
        assert!(!out[0].is_deferred());
        let out = apply_deferred_policy(vec![tool_named("Read")], &["Nope".to_string()]);
        assert!(!out[0].is_deferred());
        assert_eq!(out[0].input_schema()["required"], json!(["path"]));
    }
}
