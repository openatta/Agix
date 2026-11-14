//! `PromptAssemblyScript` — the script bound to `prompt.assemble`: the blocks
//! go in as JSON, the blocks it wants come back, and every difference is
//! applied through `PromptAssembly`.

use std::sync::Arc;

use async_trait::async_trait;

use base::interface::script::ScriptOutcome;

use crate::carrier::ScriptCarrier;

/// A script bound to the prompt-assembly hook point.
///
/// The bridge between "the operator wrote some code" and "the prompt came out
/// different": the script is handed the assembled blocks as JSON, returns the
/// blocks it wants, and every difference is applied through
/// [`PromptAssembly`](base::interface::prompt_assembly::PromptAssembly) —
/// which is what subjects it to the same authority rules as any other
/// extension. A script the operator wrote may rewrite anything; a script that
/// arrived with a downloaded plugin may add.
///
/// A script that fails, times out or returns nonsense leaves the prompt as it
/// was. That is not politeness: a prompt half-edited by a script that died
/// mid-pass is worse than an unedited one, because nothing downstream can tell
/// which it is looking at.
pub struct PromptAssemblyScript {
    carrier: Arc<ScriptCarrier>,
    entry: String,
}

impl PromptAssemblyScript {
    /// `entry` is the function the script exports, e.g. `onAssemble`.
    ///
    /// The carrier is shared rather than owned because the quota it counts is
    /// per *turn*, and something outside the adapter has to say when a turn
    /// began. A carrier moved into an adapter can never be told.
    pub fn new(carrier: Arc<ScriptCarrier>, entry: impl Into<String>) -> Self {
        Self {
            carrier,
            entry: entry.into(),
        }
    }

    /// What the script is given: one object per block, in order.
    fn encode(blocks: &[base::prompt::PromptBlock]) -> serde_json::Value {
        serde_json::Value::Array(
            blocks
                .iter()
                .map(|b| {
                    serde_json::json!({
                        "name": b.name,
                        "content": b.content,
                    })
                })
                .collect(),
        )
    }
}

/// What came back, in the shape the applier can act on.
#[derive(serde::Deserialize)]
struct ReturnedBlock {
    name: Option<String>,
    content: String,
}

#[derive(Default)]
struct Pass {
    refused: Vec<String>,
    edits: usize,
}

impl PromptAssemblyScript {
    /// Occurrences are paired in order, not matched by name alone. The
    /// kernel gives every unnamed section of a scene the same name, so a
    /// prompt normally holds several blocks called `scene.skeleton` — and
    /// an edit addressed by name lands on the first of them. Comparing the
    /// k-th returned block against the k-th existing one is what makes
    /// handing the list back unchanged cost nothing; without it, an
    /// identity pass rewrote the first section with the second's text.
    fn apply_returned<'a>(
        &self,
        assembly: &mut base::interface::prompt_assembly::PromptAssembly,
        returned: &'a [ReturnedBlock],
        existing: &[(String, String)],
        pass: &mut Pass,
    ) -> std::collections::HashMap<&'a str, usize> {
        let mut seen: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
        for block in returned {
            let Some(name) = block.name.as_deref() else {
                continue;
            };
            let occurrences: Vec<&String> = existing
                .iter()
                .filter(|(n, _)| n == name)
                .map(|(_, c)| c)
                .collect();
            let nth = seen.entry(name).or_insert(0);
            let index = *nth;
            *nth += 1;

            match occurrences.get(index) {
                // Handing a block back unchanged is not an edit, and must not
                // be charged as one: a script that returns the whole list to
                // change one line would otherwise need `modify` for all of it.
                Some(content) if **content == block.content => {}
                // An edit to one of several blocks sharing a name cannot be
                // expressed: the assembly addresses by name and would apply it
                // to the first. Refused rather than misapplied — a script that
                // silently edited the wrong section would be worse than one
                // that was told it could not.
                Some(_) if occurrences.len() > 1 => pass.refused.push(format!(
                    "`{name}` names {} blocks, so an edit to one of them cannot be addressed",
                    occurrences.len()
                )),
                Some(_) => match assembly.modify(name, block.content.clone()) {
                    Ok(_) => pass.edits += 1,
                    Err(e) => pass.refused.push(e.to_string()),
                },
                None => {
                    assembly.push(
                        base::prompt::PromptBlock::system(block.content.clone())
                            .named(name)
                            .from_origin(self.carrier.origin().clone()),
                    );
                    pass.edits += 1;
                }
            }
        }
        seen
    }

    fn record_pass(&self, pass: &Pass) {
        for denial in &pass.refused {
            self.carrier.record(
                &self.entry,
                ScriptOutcome::Refused {
                    detail: denial.clone(),
                },
            );
        }
        self.carrier.record(
            &self.entry,
            if pass.edits > 0 {
                ScriptOutcome::Applied
            } else {
                ScriptOutcome::NoChange { detail: None }
            },
        );
    }
}

/// A name is removed when fewer blocks came back under it than went out.
/// Which one of several would go is unanswerable for the same reason an edit
/// to one of them is.
fn apply_removals(
    assembly: &mut base::interface::prompt_assembly::PromptAssembly,
    existing: &[(String, String)],
    seen: &std::collections::HashMap<&str, usize>,
    pass: &mut Pass,
) {
    for name in existing
        .iter()
        .map(|(n, _)| n.as_str())
        .collect::<std::collections::BTreeSet<_>>()
    {
        let went_out = existing.iter().filter(|(n, _)| n == name).count();
        let came_back = seen.get(name).copied().unwrap_or(0);
        if came_back >= went_out {
            continue;
        }
        if went_out > 1 {
            pass.refused.push(format!(
                "`{name}` names {went_out} blocks, so removing one of them cannot be addressed"
            ));
            continue;
        }
        match assembly.remove(name) {
            Ok(_) => pass.edits += 1,
            Err(e) => pass.refused.push(e.to_string()),
        }
    }
}

#[async_trait]
impl base::interface::prompt_assembly::AsyncAssemblyHook for PromptAssemblyScript {
    async fn on_assemble_async(
        &self,
        assembly: &mut base::interface::prompt_assembly::PromptAssembly,
        _ctx: &base::interface::scene::ScenePromptContext<'_>,
    ) -> Result<(), String> {
        let before = Self::encode(assembly.blocks());
        let returned = match self.carrier.call(&self.entry, before).await {
            Ok(v) => v,
            Err(error) => {
                let message = error.to_string();
                self.carrier
                    .record(&self.entry, ScriptOutcome::Failed { error });
                return Err(message);
            }
        };

        let returned: Vec<ReturnedBlock> = match serde_json::from_value(returned) {
            Ok(blocks) => blocks,
            Err(e) => {
                let message = format!("script returned something that is not a block list: {e}");
                self.carrier.record(
                    &self.entry,
                    ScriptOutcome::NoChange {
                        detail: Some(message.clone()),
                    },
                );
                return Err(message);
            }
        };

        // Diffed rather than replaced wholesale, so every change goes through
        // the authority checks one at a time. Handing back a list and swapping
        // it in would let a script "add" a prompt that happens to be missing
        // the block it was not allowed to remove.
        let existing: Vec<(String, String)> = assembly
            .blocks()
            .iter()
            .filter_map(|b| b.name.clone().map(|n| (n, b.content.clone())))
            .collect();

        // A denied edit does not cancel the permitted ones. A script that
        // asked for one thing it may not do still gets everything it may —
        // the alternative punishes an author for a single overreach by
        // silently dropping the rest of their pass.
        let mut pass = Pass::default();
        let seen = self.apply_returned(assembly, &returned, &existing, &mut pass);
        apply_removals(assembly, &existing, &seen, &mut pass);
        self.record_pass(&pass);

        if pass.refused.is_empty() {
            Ok(())
        } else {
            Err(pass.refused.join("; "))
        }
    }
}

#[cfg(test)]
mod prompt_hook_tests {
    use super::*;
    use base::interface::prompt_assembly::{
        run_async_assembly_hooks, AssemblyCapabilities, AsyncAssemblyHook, Authority,
    };
    use base::interface::script::{
        FnScriptEngine, ScriptEngine, ScriptError, ScriptLimits, ScriptSource,
    };
    use base::prompt::{names, BlockOrigin, PromptBlock};
    use std::time::Duration;

    fn ctx() -> base::interface::scene::ScenePromptContext<'static> {
        use std::borrow::Cow;
        let mut c = base::interface::scene::ScenePromptContext::default();
        c.cwd = Cow::Borrowed("/tmp");
        c.os = Cow::Borrowed("linux");
        c.shell = Cow::Borrowed("bash");
        c.home_dir = Cow::Borrowed("/home/user");
        c.date = Cow::Borrowed("2026-06-10");
        c.model_name = Cow::Borrowed("test-model");
        c
    }

    fn prompt() -> Vec<PromptBlock> {
        vec![
            PromptBlock::system("you are an agent").named(names::SCENE_SKELETON),
            PromptBlock::system("skills: a, b").named(names::SKILLS_CATALOG),
            PromptBlock::system("rules: no rm -rf").named(names::RULES),
        ]
    }

    /// Stands in for a real interpreter: it receives the blocks as JSON,
    /// transforms them, and hands them back — exactly the contract a scripted
    /// `onAssemble` would satisfy.
    fn engine_that(
        f: impl Fn(Vec<serde_json::Value>) -> Vec<serde_json::Value> + Send + Sync + Clone + 'static,
    ) -> Arc<dyn ScriptEngine> {
        Arc::new(FnScriptEngine(
            move |_s: &ScriptSource, _e: &str, input: serde_json::Value| {
                let f = f.clone();
                async move {
                    let blocks: Vec<serde_json::Value> = serde_json::from_value(input)
                        .map_err(|e| ScriptError::Failed(e.to_string()))?;
                    Ok(serde_json::Value::Array(f(blocks)))
                }
            },
        ))
    }

    fn script_hook(engine: Arc<dyn ScriptEngine>, origin: BlockOrigin) -> PromptAssemblyScript {
        PromptAssemblyScript::new(
            Arc::new(ScriptCarrier::new(
                engine,
                ScriptSource {
                    id: "./.agix/scripts/prompt.js".into(),
                    origin,
                    code: String::new(),
                },
                "prompt.assemble",
                ScriptLimits::default(),
            )),
            "onAssemble",
        )
    }

    /// Several blocks can share a name, and handing them all back unchanged
    /// must leave every one of them alone.
    ///
    /// The kernel gives every unnamed section of a scene the same name, so a
    /// real prompt holds several `scene.skeleton` blocks. Matching a returned
    /// block to an existing one by name alone finds the first every time: the
    /// second and third sections then read as edits *to the first*, and a
    /// script that asked for nothing rewrote the opening of the system prompt
    /// with the text of a later section. Occurrences are paired in order for
    /// this reason.
    #[tokio::test]
    async fn several_blocks_can_share_a_name_and_an_identity_pass_leaves_them_all() {
        let mut assembly = base::interface::prompt_assembly::PromptAssembly::new(
            vec![
                PromptBlock::system("first section").named(names::SCENE_SKELETON),
                PromptBlock::system("second section").named(names::SCENE_SKELETON),
                PromptBlock::system("third section").named(names::SCENE_SKELETON),
            ],
            Authority::local(BlockOrigin::Script("./.agix/scripts/prompt.js".into())),
        );

        let hook = script_hook(
            engine_that(|blocks| blocks),
            BlockOrigin::Script("./.agix/scripts/prompt.js".into()),
        );
        hook.on_assemble_async(&mut assembly, &ctx())
            .await
            .expect("an identity pass asks for nothing and cannot be refused");

        let contents: Vec<&str> = assembly
            .blocks()
            .iter()
            .map(|b| b.content.as_str())
            .collect();
        assert_eq!(
            contents,
            vec!["first section", "second section", "third section"],
            "a pass that changed nothing rewrote the prompt"
        );
    }

    /// And an edit to one of them is refused rather than applied to whichever
    /// came first.
    #[tokio::test]
    async fn an_edit_to_one_of_several_blocks_sharing_a_name_is_refused() {
        let mut assembly = base::interface::prompt_assembly::PromptAssembly::new(
            vec![
                PromptBlock::system("first section").named(names::SCENE_SKELETON),
                PromptBlock::system("second section").named(names::SCENE_SKELETON),
            ],
            Authority::local(BlockOrigin::Script("./.agix/scripts/prompt.js".into())),
        );

        let hook = script_hook(
            engine_that(|mut blocks| {
                blocks[1]["content"] = serde_json::json!("second section, edited");
                blocks
            }),
            BlockOrigin::Script("./.agix/scripts/prompt.js".into()),
        );
        let refused = hook
            .on_assemble_async(&mut assembly, &ctx())
            .await
            .expect_err("an edit nothing can address must be reported");
        assert!(refused.contains("names 2 blocks"), "{refused}");

        let contents: Vec<&str> = assembly
            .blocks()
            .iter()
            .map(|b| b.content.as_str())
            .collect();
        assert_eq!(
            contents,
            vec!["first section", "second section"],
            "the edit was applied to a block the script did not name"
        );
    }

    /// The acceptance case: a script rewrites a prompt block, and the prompt
    /// that goes out is different. No recompilation of the engine involved —
    /// the code came from outside it.
    #[tokio::test]
    async fn a_script_the_operator_wrote_rewrites_a_block() {
        let engine = engine_that(|blocks| {
            blocks
                .into_iter()
                .map(|mut b| {
                    if b["name"] == serde_json::json!(names::SKILLS_CATALOG) {
                        b["content"] = serde_json::json!("skills: (curated by script)");
                    }
                    b
                })
                .collect()
        });
        let hook = script_hook(
            engine,
            BlockOrigin::Script("./.agix/scripts/prompt.js".into()),
        );
        let hooks: Vec<(Arc<dyn AsyncAssemblyHook>, Authority)> = vec![(
            Arc::new(hook),
            Authority::local(BlockOrigin::Script("./.agix/scripts/prompt.js".into())),
        )];

        let out = run_async_assembly_hooks(prompt(), &hooks, &ctx()).await;
        let skills = out
            .iter()
            .find(|b| b.name.as_deref() == Some(names::SKILLS_CATALOG))
            .unwrap();
        assert_eq!(skills.content, "skills: (curated by script)");
    }

    /// The same script, arriving with a downloaded plugin that declared
    /// nothing: its additions land, its deletion does not.
    #[tokio::test]
    async fn a_downloaded_script_is_held_to_the_same_rules_as_any_plugin() {
        let engine = engine_that(|blocks| {
            let mut kept: Vec<serde_json::Value> = blocks
                .into_iter()
                .filter(|b| b["name"] != serde_json::json!(names::RULES))
                .collect();
            kept.push(serde_json::json!({"name": "example.note", "content": "hello"}));
            kept
        });
        let hook = script_hook(engine, BlockOrigin::Plugin("example".into()));
        let hooks: Vec<(Arc<dyn AsyncAssemblyHook>, Authority)> = vec![(
            Arc::new(hook),
            Authority::plugin("example", AssemblyCapabilities::default()),
        )];

        let out = run_async_assembly_hooks(prompt(), &hooks, &ctx()).await;
        let names_out: Vec<&str> = out.iter().filter_map(|b| b.name.as_deref()).collect();
        assert!(
            names_out.contains(&names::RULES),
            "an undeclared removal must not take effect: {names_out:?}"
        );
        assert!(
            names_out.contains(&"example.note"),
            "its addition must stand: {names_out:?}"
        );
    }

    #[tokio::test]
    async fn a_script_that_returns_nonsense_leaves_the_prompt_alone() {
        let engine: Arc<dyn ScriptEngine> = Arc::new(FnScriptEngine(
            |_s: &ScriptSource, _e: &str, _i: serde_json::Value| async {
                Ok(serde_json::json!("not a block list"))
            },
        ));
        let hook = script_hook(
            engine,
            BlockOrigin::Script("./.agix/scripts/prompt.js".into()),
        );
        let hooks: Vec<(Arc<dyn AsyncAssemblyHook>, Authority)> = vec![(
            Arc::new(hook),
            Authority::local(BlockOrigin::Script("./.agix/scripts/prompt.js".into())),
        )];

        let out = run_async_assembly_hooks(prompt(), &hooks, &ctx()).await;
        assert_eq!(out, prompt(), "a broken script is not a prompt edit");
    }

    #[tokio::test]
    async fn a_script_that_hangs_leaves_the_prompt_alone() {
        let engine: Arc<dyn ScriptEngine> = Arc::new(FnScriptEngine(
            |_s: &ScriptSource, _e: &str, _i: serde_json::Value| async {
                std::future::pending::<()>().await;
                unreachable!()
            },
        ));
        let hook = PromptAssemblyScript::new(
            Arc::new(ScriptCarrier::new(
                engine,
                ScriptSource {
                    id: "./.agix/scripts/slow.js".into(),
                    origin: BlockOrigin::Script("./.agix/scripts/slow.js".into()),
                    code: String::new(),
                },
                "prompt.assemble",
                ScriptLimits {
                    timeout: Duration::from_millis(20),
                    ..Default::default()
                },
            )),
            "onAssemble",
        );
        let hooks: Vec<(Arc<dyn AsyncAssemblyHook>, Authority)> = vec![(
            Arc::new(hook),
            Authority::local(BlockOrigin::Script("./.agix/scripts/slow.js".into())),
        )];

        let out = run_async_assembly_hooks(prompt(), &hooks, &ctx()).await;
        assert_eq!(out, prompt(), "the turn goes on without the script's edits");
    }
}
