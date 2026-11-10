//! Which tools a session may have, as allow and deny lists that only ever
//! narrow: one matching semantics for a scene's lists, an agent type's and a
//! caller's. A deny always holds, whether or not an allow list is given, and
//! an MCP server is named by its configured name and matched on the tool's
//! origin, never parsed out of the tool's name.

use super::tool::ToolOrigin;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolPattern {
    Name(String),
    Prefix(String),
    McpServer(String),
    AnyMcp,
}

impl ToolPattern {
    /// `mcp__*` is every MCP tool; `mcp__s` and `mcp__s__*` are server `s`'s;
    /// `mcp__s__t` is that one tool by name; `X*` is every name starting `X`;
    /// anything else is one name.
    pub fn parse(pattern: &str) -> Self {
        if let Some(rest) = pattern.strip_prefix("mcp__") {
            if rest == "*" {
                return Self::AnyMcp;
            }
            if let Some(server) = rest.strip_suffix("__*") {
                return Self::McpServer(server.to_string());
            }
            if !rest.is_empty() && !rest.contains("__") {
                return Self::McpServer(rest.to_string());
            }
        }
        match pattern.strip_suffix('*') {
            Some(prefix) => Self::Prefix(prefix.to_string()),
            None => Self::Name(pattern.to_string()),
        }
    }

    pub fn matches(&self, name: &str, origin: &ToolOrigin) -> bool {
        match self {
            Self::Name(n) => name == n,
            Self::Prefix(p) => name.starts_with(p.as_str()),
            Self::McpServer(s) => origin.mcp_server() == Some(s.as_str()),
            Self::AnyMcp => origin.is_mcp(),
        }
    }

    /// Every tool `other` matches, this matches too.
    fn covers(&self, other: &Self) -> bool {
        match (self, other) {
            (a, b) if a == b => true,
            (Self::AnyMcp, Self::McpServer(_)) => true,
            (Self::Prefix(p), Self::Prefix(q)) => q.starts_with(p.as_str()),
            (Self::Prefix(p), Self::Name(n)) => n.starts_with(p.as_str()),
            _ => false,
        }
    }

    /// No tool matches both. Unknowable for a name against an origin, so
    /// those are taken to overlap.
    fn disjoint(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Name(a), Self::Name(b)) => a != b,
            (Self::Name(n), Self::Prefix(p)) | (Self::Prefix(p), Self::Name(n)) => {
                !n.starts_with(p.as_str())
            }
            (Self::Prefix(a), Self::Prefix(b)) => {
                !a.starts_with(b.as_str()) && !b.starts_with(a.as_str())
            }
            (Self::McpServer(a), Self::McpServer(b)) => a != b,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Layer {
    allow: Option<Vec<ToolPattern>>,
    deny: Vec<ToolPattern>,
}

impl Layer {
    fn admits(&self, name: &str, origin: &ToolOrigin) -> bool {
        let hit = |patterns: &[ToolPattern]| patterns.iter().any(|p| p.matches(name, origin));
        !hit(&self.deny) && self.allow.as_deref().is_none_or(hit)
    }

    fn within(&self, parent: &Layer) -> bool {
        let allowed_within = match (&self.allow, &parent.allow) {
            (_, None) => true,
            (None, Some(_)) => false,
            (Some(mine), Some(theirs)) => mine.iter().all(|m| theirs.iter().any(|t| t.covers(m))),
        };
        let denied_within = parent.deny.iter().all(|d| {
            self.deny.iter().any(|mine| mine.covers(d))
                || self
                    .allow
                    .as_ref()
                    .is_some_and(|mine| mine.iter().all(|m| m.disjoint(d)))
        });
        allowed_within && denied_within
    }
}

/// Layers of allow and deny lists; a tool is admitted when every layer
/// admits it. No layers admits every tool.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolSelector {
    layers: Vec<Layer>,
}

impl ToolSelector {
    pub fn all() -> Self {
        Self::default()
    }

    /// One layer. `allow: None` allows every tool the deny list leaves.
    pub fn layer(allow: Option<Vec<ToolPattern>>, deny: Vec<ToolPattern>) -> Self {
        Self {
            layers: vec![Layer { allow, deny }],
        }
    }

    /// One layer from lists as configured, an empty allow list allowing every
    /// tool.
    pub fn from_lists(allowed: &[String], disallowed: &[String]) -> Self {
        let parsed = |list: &[String]| list.iter().map(|p| ToolPattern::parse(p)).collect();
        if allowed.is_empty() && disallowed.is_empty() {
            return Self::all();
        }
        Self::layer(
            (!allowed.is_empty()).then(|| parsed(allowed)),
            parsed(disallowed),
        )
    }

    pub fn admits(&self, name: &str, origin: &ToolOrigin) -> bool {
        self.layers.iter().all(|l| l.admits(name, origin))
    }

    /// Both selectors' layers: admits only what both admit.
    pub fn and(mut self, other: ToolSelector) -> Self {
        self.layers.extend(other.layers);
        self
    }

    /// Whether everything this admits, `parent` admits too, as far as the
    /// patterns alone can show; `false` when they cannot.
    pub fn within(&self, parent: &ToolSelector) -> bool {
        parent
            .layers
            .iter()
            .all(|p| self.layers.iter().any(|mine| mine.within(p)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lists(allowed: &[&str], disallowed: &[&str]) -> ToolSelector {
        let owned = |l: &[&str]| l.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        ToolSelector::from_lists(&owned(allowed), &owned(disallowed))
    }

    fn builtin() -> ToolOrigin {
        ToolOrigin::Builtin
    }

    #[test]
    fn patterns_parse_by_their_shape() {
        use ToolPattern::*;
        assert_eq!(ToolPattern::parse("mcp__*"), AnyMcp);
        assert_eq!(ToolPattern::parse("mcp__docs"), McpServer("docs".into()));
        assert_eq!(
            ToolPattern::parse("mcp__my.docs"),
            McpServer("my.docs".into())
        );
        assert_eq!(ToolPattern::parse("mcp__docs__*"), McpServer("docs".into()));
        assert_eq!(
            ToolPattern::parse("mcp__docs__search"),
            Name("mcp__docs__search".into())
        );
        assert_eq!(
            ToolPattern::parse("mcp__docs__se*"),
            Prefix("mcp__docs__se".into())
        );
        assert_eq!(ToolPattern::parse("Task*"), Prefix("Task".into()));
        assert_eq!(ToolPattern::parse("Read"), Name("Read".into()));
        assert_eq!(ToolPattern::parse("mcp__"), Name("mcp__".into()));
    }

    #[test]
    fn a_server_is_matched_on_the_origin_not_the_tool_name() {
        let server = ToolPattern::parse("mcp__my.docs");
        assert!(server.matches("mcp__my_docs__search", &ToolOrigin::mcp("my.docs")));
        assert!(!server.matches("mcp__my_docs__search", &ToolOrigin::mcp("my_docs")));
        assert!(!server.matches("mcp__my.docs__search", &builtin()));
        let any = ToolPattern::AnyMcp;
        assert!(any.matches("whatever", &ToolOrigin::mcp("s")));
        assert!(!any.matches("mcp__s__t", &builtin()));
        assert!(!any.matches("mcp__s__t", &ToolOrigin::plugin("p")));
    }

    #[test]
    fn a_deny_holds_with_or_without_an_allow_list() {
        let denies_bash = lists(&[], &["Bash"]);
        assert!(denies_bash.admits("Read", &builtin()));
        assert!(!denies_bash.admits("Bash", &builtin()));
        let both = lists(&["Bash", "Read"], &["Bash"]);
        assert!(both.admits("Read", &builtin()));
        assert!(!both.admits("Bash", &builtin()));
        assert!(!both.admits("Write", &builtin()));
    }

    #[test]
    fn no_lists_admit_everything() {
        assert!(ToolSelector::all().admits("Anything", &ToolOrigin::mcp("s")));
        assert_eq!(lists(&[], &[]), ToolSelector::all());
    }

    #[test]
    fn layers_only_narrow() {
        let scene = lists(&["Read", "Grep", "Bash"], &[]);
        let caller = lists(&[], &["Bash"]);
        let both = scene.clone().and(caller);
        assert!(both.admits("Read", &builtin()));
        assert!(!both.admits("Bash", &builtin()));
        assert!(!both.admits("Write", &builtin()));
        assert!(!scene
            .and(lists(&["Write"], &[]))
            .admits("Write", &builtin()));
    }

    #[test]
    fn a_narrower_list_is_within_a_wider_one() {
        let parent = lists(&["Read", "Grep", "Bash"], &[]);
        assert!(lists(&["Read", "Grep"], &[]).within(&parent));
        assert!(!lists(&["Read", "Write"], &[]).within(&parent));
        assert!(
            !lists(&[], &[]).within(&parent),
            "everything is not within a list"
        );
        assert!(lists(&[], &[]).within(&ToolSelector::all()));
        assert!(lists(&["Read"], &[]).within(&ToolSelector::all()));
    }

    #[test]
    fn within_a_deny_list_means_denying_it_or_never_allowing_it() {
        let parent = lists(&[], &["Bash"]);
        assert!(lists(&["Read"], &[]).within(&parent));
        assert!(lists(&[], &["Bash", "Write"]).within(&parent));
        assert!(!lists(&["Read", "Bash"], &[]).within(&parent));
        assert!(!lists(&[], &[]).within(&parent));
        assert!(lists(&[], &["B*"]).within(&parent));
    }

    #[test]
    fn prefixes_and_servers_cover_what_they_name() {
        assert!(lists(&["TaskStop"], &[]).within(&lists(&["Task*"], &[])));
        assert!(lists(&["TaskS*"], &[]).within(&lists(&["Task*"], &[])));
        assert!(!lists(&["Ta*"], &[]).within(&lists(&["Task*"], &[])));
        assert!(lists(&["mcp__docs"], &[]).within(&lists(&["mcp__*"], &[])));
        assert!(!lists(&["mcp__docs__search"], &[]).within(&lists(&["mcp__docs"], &[])));
        assert!(lists(&["mcp__a"], &[]).within(&lists(&[], &["mcp__b"])));
        assert!(!lists(&["Read"], &[]).within(&lists(&[], &["mcp__b"])));
    }

    #[test]
    fn within_holds_layer_by_layer() {
        let parent = lists(&["Read", "Bash"], &[]).and(lists(&[], &["Bash"]));
        assert!(lists(&["Read"], &[]).within(&parent));
        assert!(!lists(&["Bash"], &[]).within(&parent));
        let child = lists(&["Read", "Bash"], &[]).and(lists(&[], &["Bash"]));
        assert!(child.within(&parent));
    }
}
