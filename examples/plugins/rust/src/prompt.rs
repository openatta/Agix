//! The prompt contributions: a fixed block, a computed one, two variables, and
//! a pass of each kind over the assembled prompt.
//!
//! Every block is interpolated before the passes run, so `{{demo_rust_project}}`
//! in the guide and `{{demo_rust_turn}}` in the activity block are filled in by
//! the time the outline reads them.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use async_trait::async_trait;
use base::interface::prompt_assembly::{
    AssemblyCapabilities, AssemblyHook, AsyncAssemblyHook, Authority, PromptAssembly,
};
use base::interface::prompt_registry::RegisteredBlock;
use base::interface::scene::ScenePromptContext;
use base::prompt::{BlockOrigin, PromptBlock};
use runtime::plugin::Registrar;

use crate::activity::Activity;

pub const GUIDE: &str = "demo-rust.guide";
pub const ACTIVITY: &str = "demo-rust.activity";
pub const OUTLINE: &str = "demo-rust.outline";
pub const NOTICE: &str = "demo-rust.notice";
pub const NOTICE_FILE: &str = ".demo/notice.md";

pub fn register(r: &mut Registrar, project_root: Option<&Path>, activity: Arc<Activity>) {
    r.prompt_block(RegisteredBlock::system(
        GUIDE,
        450,
        format!(
            "This session runs the {plugin} plugin in project `{{{{demo_rust_project}}}}`. \
             The {tool} tool returns the stamp of a text. Files ending in `{suffix}` are off \
             limits, and tokens shaped like SECRET-… are redacted before you see them; say so \
             rather than guessing what they were.",
            plugin = crate::NAME,
            tool = crate::tool::NAME,
            suffix = crate::SECRET_SUFFIX,
        ),
    ))
    .prompt_variable(
        "demo_rust_project",
        Arc::new(|ctx: &ScenePromptContext<'_>| {
            Path::new(ctx.cwd.as_ref())
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        }),
    );

    let counted = activity.clone();
    r.prompt_context(
        ACTIVITY,
        900,
        Arc::new(move |_: &ScenePromptContext<'_>| {
            Some(format!(
                "{}: turn {{{{demo_rust_turn}}}}; {} stamps, {} refused calls, {} redactions so far.",
                crate::NAME,
                counted.stamps.load(Ordering::Relaxed),
                counted.refused.load(Ordering::Relaxed),
                counted.redacted.load(Ordering::Relaxed),
            ))
        }),
    );
    let turns = activity.clone();
    r.prompt_variable_from(
        "demo_rust_turn",
        Arc::new(move |_: &ScenePromptContext<'_>| {
            Some(turns.turns.load(Ordering::Relaxed).to_string())
        }),
        BlockOrigin::Plugin(crate::NAME.into()),
    );

    r.assembly_hook(Arc::new(Outline));
    if let Some(project_root) = project_root {
        r.async_assembly_hook(
            Arc::new(ProjectNotice {
                file: project_root.join(NOTICE_FILE),
                activity,
            }),
            Authority::plugin(crate::NAME, AssemblyCapabilities::default()),
        );
    }
}

/// The section names, in the order the model reads them.
struct Outline;

impl AssemblyHook for Outline {
    fn on_assemble(
        &self,
        assembly: &mut PromptAssembly,
        _ctx: &ScenePromptContext<'_>,
    ) -> Result<(), String> {
        let names: Vec<&str> = assembly
            .blocks()
            .iter()
            .filter_map(|b| b.name.as_deref())
            .collect();
        let outline = format!("Sections of this prompt, in order: {}.", names.join(", "));
        assembly.push(PromptBlock::system(outline).named(OUTLINE));
        Ok(())
    }
}

/// The project's own notice, read from disk every turn — the reason this pass
/// is the async kind. It runs with a plugin's authority, which may add a block
/// and nothing else.
struct ProjectNotice {
    file: PathBuf,
    activity: Arc<Activity>,
}

#[async_trait]
impl AsyncAssemblyHook for ProjectNotice {
    async fn on_assemble_async(
        &self,
        assembly: &mut PromptAssembly,
        _ctx: &ScenePromptContext<'_>,
    ) -> Result<(), String> {
        let text = match tokio::fs::read_to_string(&self.file).await {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.activity.note_notice_error(None);
                return Ok(());
            }
            Err(e) => {
                self.activity.note_notice_error(Some(e.to_string()));
                return Err(e.to_string());
            }
        };
        self.activity.note_notice_error(None);
        let text = text.trim();
        if !text.is_empty() {
            assembly.push(
                PromptBlock::system(format!("Notice from this project ({NOTICE_FILE}):\n{text}"))
                    .named(NOTICE)
                    .from_origin(BlockOrigin::Plugin(crate::NAME.into())),
            );
        }
        Ok(())
    }
}
