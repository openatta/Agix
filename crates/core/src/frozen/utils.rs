//! String utility functions used across the `frozen` module.

const MEMORY_ENTRYPOINT_MAX_LINES: usize = 200;
const MEMORY_ENTRYPOINT_MAX_BYTES: usize = 25_000;

/// Truncate a string at `max` chars, appending `marker` when truncated.
pub(crate) fn truncate_chars(s: &str, max: usize, marker: &str) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max).collect();
        format!("{head}{marker}")
    }
}

/// Truncate MEMORY.md content by line count and byte limit.
pub(crate) fn truncate_memory_entrypoint(raw: &str) -> String {
    let trimmed = raw.trim();
    let lines: Vec<&str> = trimmed.lines().collect();
    let was_line_truncated = lines.len() > MEMORY_ENTRYPOINT_MAX_LINES;
    let was_byte_truncated = trimmed.len() > MEMORY_ENTRYPOINT_MAX_BYTES;

    if !was_line_truncated && !was_byte_truncated {
        return trimmed.to_string();
    }

    let mut out = if was_line_truncated {
        lines[..MEMORY_ENTRYPOINT_MAX_LINES].join("\n")
    } else {
        trimmed.to_string()
    };
    if out.len() > MEMORY_ENTRYPOINT_MAX_BYTES {
        let cut = out
            .char_indices()
            .take_while(|(idx, _)| *idx <= MEMORY_ENTRYPOINT_MAX_BYTES)
            .map(|(idx, _)| idx)
            .last()
            .unwrap_or(MEMORY_ENTRYPOINT_MAX_BYTES);
        let newline = out[..cut].rfind('\n').unwrap_or(cut);
        out.truncate(newline);
    }

    let reason = match (was_line_truncated, was_byte_truncated) {
        (true, true) => format!("{} lines and {} bytes", lines.len(), trimmed.len()),
        (true, false) => format!(
            "{} lines (limit: {})",
            lines.len(),
            MEMORY_ENTRYPOINT_MAX_LINES
        ),
        (false, true) => format!(
            "{} bytes (limit: {})",
            trimmed.len(),
            MEMORY_ENTRYPOINT_MAX_BYTES
        ),
        (false, false) => unreachable!(),
    };
    out.push_str(&format!(
        "\n\n> WARNING: MEMORY.md is {reason}. Only part of it was loaded. Keep index entries to one line and move detail into topic files."
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_long_string() {
        let s: String = "x".repeat(3000);
        let t = truncate_chars(&s, 100, "[truncated]");
        assert_eq!(t.chars().count(), 100 + "[truncated]".chars().count());
        assert!(t.ends_with("[truncated]"));
    }

    #[test]
    fn truncate_short_string_is_passthrough() {
        let t = truncate_chars("hi", 10, "[truncated]");
        assert_eq!(t, "hi");
    }

    #[test]
    fn memory_entrypoint_truncates_by_line_count() {
        let raw = (0..250)
            .map(|i| format!("- item {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let t = truncate_memory_entrypoint(&raw);
        assert!(t.contains("WARNING: MEMORY.md"));
        assert!(t.contains("item 199"));
        assert!(!t.contains("item 220"));
    }

    #[test]
    fn memory_entrypoint_truncates_by_byte_count() {
        let raw = format!("- {}\n- tail", "x".repeat(30_000));
        let t = truncate_memory_entrypoint(&raw);
        assert!(t.contains("WARNING: MEMORY.md"));
        assert!(t.len() < raw.len());
    }
}
