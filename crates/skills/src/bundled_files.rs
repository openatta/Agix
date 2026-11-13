//! Reads a skill's `files:` frontmatter (reference material bundled beside
//! SKILL.md) into the text appended to its expansion, with the folder-escape
//! checks and the size budgets that apply to it.

/// Per-file cap for bundled reference material (`files:` frontmatter) — see
/// `build_bundled_files_injection`.
const BUNDLED_FILE_CHAR_CAP: usize = 2000;
/// Total cap across all of a skill's bundled files combined, separate from
/// (and on top of) the skill body's own 8000-char cap.
const BUNDLED_TOTAL_CHAR_CAP: usize = 6000;

/// Resolves, safety-validates, reads, and budget-caps a skill's `files:`
/// frontmatter (reference material bundled alongside SKILL.md), returning
/// ready-to-append injection text on success.
///
/// `Err(message)` is returned — naming both the offending skill and the
/// declared path — when a declared file escapes the skill's own folder
/// (e.g. via `..`) or doesn't exist / can't be read. These are surfaced as
/// visible tool errors by the caller, never silently skipped.
///
/// `skill_path` is the skill's SKILL.md path (`SkillInfo::path`). This
/// re-reads and re-parses that file directly rather than widening
/// `SkillManager::list()`'s `SkillInfo` projection with a `files` field:
/// `SkillInfo` is a runtime-facing view of `SkillEntry` used across the
/// whole `skills` crate, and `SkillEntry` (which already carries `files`,
/// see `crates/core/src/frozen/skill.rs`) is cheaply reconstructed here with
/// a single read-plus-parse, cheaper than a cross-crate struct change for
/// one field only this call site needs. Bundled/MCP sentinel skills
/// (paths like `(bundled:name)` or `(mcp:...)`) simply fail this read and
/// fall through to "no bundled files," which is correct — those skills
/// have no real folder to bundle files alongside.
async fn reparse_skill_entry(
    skill_path: &std::path::Path,
    fs: &dyn base::interface::exec::FileSystem,
    files: &base::paths::PromptFiles,
) -> Option<base::frozen::skill::SkillEntry> {
    let real_skill_path = fs.canonicalize(skill_path).await.ok()?;
    files.admit(skill_path, &real_skill_path).ok()?;
    let content = fs.read_to_string(&real_skill_path).await.ok()?;
    let dir_name = skill_path
        .parent()
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    base::frozen::frontmatter::parse_skill_file(
        &content,
        dir_name,
        skill_path,
        base::frozen::skill::SkillSource::Project,
    )
}

/// The resolved real path must still be inside the skill's own
/// (canonicalized) folder, which is what rejects `..` traversal. A failed
/// canonicalize (missing or unreadable file) is reported the same way: a
/// named, visible error, not a silent skip.
async fn read_bundled_file(
    fs: &dyn base::interface::exec::FileSystem,
    files: &base::paths::PromptFiles,
    skill_name: &str,
    relative: &str,
    absolute: &std::path::Path,
    canonical_skill_dir: &std::path::Path,
) -> Result<String, String> {
    let real_path = fs.canonicalize(absolute).await.map_err(|e| {
        format!(
            "Skill '{skill_name}' declares bundled file `{relative}` \
             (resolved to {}) which does not exist or could not be read: {e}",
            absolute.display()
        )
    })?;
    if !permissions::path_rule::is_within_root_lexical(&real_path, canonical_skill_dir) {
        return Err(format!(
            "Skill '{skill_name}' declares bundled file `{relative}` which resolves \
             outside the skill's own folder ({}): {} — rejected.",
            canonical_skill_dir.display(),
            real_path.display()
        ));
    }
    if let Err(why) = files.admit(absolute, &real_path) {
        return Err(format!(
            "Skill '{skill_name}' declares bundled file `{relative}`: {why}"
        ));
    }
    fs.read_to_string(&real_path).await.map_err(|e| {
        format!(
            "Skill '{skill_name}' declares bundled file `{relative}` \
             which could not be read: {e}"
        )
    })
}

pub(crate) async fn build_bundled_files_injection(
    skill_path: &std::path::Path,
    skill_name: &str,
    fs: &dyn base::interface::exec::FileSystem,
    files: &base::paths::PromptFiles,
) -> Result<String, String> {
    let Some(entry) = reparse_skill_entry(skill_path, fs, files).await else {
        return Ok(String::new());
    };

    let declared = entry.files.clone().unwrap_or_default();
    if declared.is_empty() {
        return Ok(String::new());
    }
    let candidates = entry.resolve_bundled_files();

    let Some(skill_dir) = skill_path.parent() else {
        return Ok(String::new());
    };
    // Canonicalize once: the root every candidate must stay inside.
    let Ok(canonical_skill_dir) = fs.canonicalize(skill_dir).await else {
        return Ok(String::new());
    };

    let mut out = String::new();
    let mut total_used = 0usize;
    for (relative, absolute) in declared.iter().zip(candidates.iter()) {
        if total_used >= BUNDLED_TOTAL_CHAR_CAP {
            out.push_str(&format!(
                "\n\n---\n## Bundled file: {relative}\n\n\
                 [skipped: total bundled-files budget ({BUNDLED_TOTAL_CHAR_CAP} chars) \
                 already reached by earlier files]\n"
            ));
            continue;
        }

        let file_content = read_bundled_file(
            fs,
            files,
            skill_name,
            relative,
            absolute,
            &canonical_skill_dir,
        )
        .await?;

        // Caps are char counts, not byte counts — slicing by raw byte index
        // (`file_content.len()` / `&file_content[..n]`) panics on non-ASCII
        // content whose cut point doesn't land on a UTF-8 char boundary
        // (very reachable here: skill reference docs in this repo are
        // routinely Chinese). `char_indices().nth(n)` finds the byte offset
        // of the n-th character, giving a boundary-safe cut.
        let per_file_cap = BUNDLED_FILE_CHAR_CAP.min(BUNDLED_TOTAL_CHAR_CAP - total_used);
        let (chunk, was_truncated) = match file_content.char_indices().nth(per_file_cap) {
            Some((byte_idx, _)) => (&file_content[..byte_idx], true),
            None => (file_content.as_str(), false),
        };
        total_used += chunk.chars().count();

        out.push_str(&format!("\n\n---\n## Bundled file: {relative}\n\n{chunk}"));
        if was_truncated {
            out.push_str("...[truncated]");
        }
        out.push('\n');
    }
    Ok(out)
}
