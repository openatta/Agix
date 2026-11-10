//! JSON-RPC 错误码：标准码加 daemon 的扩展。`ErrorCode::ALL` 是全集，schema 和
//! 协议文档 §9 的对账都从它读；同名的 `i32` 常量给构造响应的地方用。

macro_rules! error_codes {
    ($( $(#[$doc:meta])* $variant:ident = $code:literal, $name:ident; )*) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        #[repr(i32)]
        pub enum ErrorCode {
            $( $(#[$doc])* $variant = $code, )*
        }

        impl ErrorCode {
            pub const ALL: &'static [Self] = &[$(Self::$variant),*];

            pub const fn code(self) -> i32 {
                self as i32
            }

            pub const fn name(self) -> &'static str {
                match self {
                    $(Self::$variant => stringify!($name),)*
                }
            }
        }

        $( pub const $name: i32 = $code; )*
    };
}

error_codes! {
    ParseError = -32700, PARSE_ERROR;
    InvalidRequest = -32600, INVALID_REQUEST;
    MethodNotFound = -32601, METHOD_NOT_FOUND;
    InvalidParams = -32602, INVALID_PARAMS;
    InternalError = -32603, INTERNAL_ERROR;
    SessionNotFound = -32000, SESSION_NOT_FOUND;
    EngineError = -32002, ENGINE_ERROR;
    /// TCP handshake (`daemon.auth`) missing, malformed, or wrong token.
    Unauthorized = -32003, UNAUTHORIZED;
    /// `session.resume`/`session.fork`: the session's recorded `Meta.scene`
    /// does not match the scene being resumed into. See
    /// `docs/daemon_rpc_protocol.md` §9.1 for the `data` shape this carries.
    SceneMismatch = -32006, SCENE_MISMATCH;
    /// `session.create`: the given `project_root` doesn't exist or isn't a
    /// readable directory. See `docs/daemon_rpc_protocol.md` §9.1 for the
    /// `data` shape this carries.
    ProjectNotFound = -32010, PROJECT_NOT_FOUND;
    /// `session.create`/`scene.activate`: the given scene isn't one this
    /// daemon binary registers at all.
    SceneNotFound = -32004, SCENE_NOT_FOUND;
    /// `scene.deactivate`: refused because sessions are still recorded
    /// under this scene. See `docs/daemon_rpc_protocol.md` §9.1 for the
    /// `data` shape this carries.
    SceneHasActiveSessions = -32009, SCENE_HAS_ACTIVE_SESSIONS;
    /// `session.resume`: the target is a sidechain that already ran its
    /// one-shot task to conclusion — no continuation semantics, re-derive a
    /// new sub-agent instead. See `docs/daemon_rpc_protocol.md` §9.1 for the
    /// `data` shape this carries.
    SidechainTerminal = -32014, SIDECHAIN_TERMINAL;
    /// Any `plugin.*` management call on a build where the plugin subsystem
    /// is unavailable — compiled out (`--no-default-features`) or switched
    /// off by policy. Distinct from `METHOD_NOT_FOUND`: the method exists and
    /// the request was well-formed; the capability is absent by deployment
    /// choice, which is what a client needs to be able to tell apart.
    PluginsDisabled = -32016, PLUGINS_DISABLED;
    /// This connection already has `MAX_IN_FLIGHT_PER_CONNECTION` requests
    /// being served and will not start another.
    ///
    /// Refused rather than queued, because queuing would mean the connection
    /// stops reading — the exact failure this bound exists to prevent. A
    /// client that sees this has more outstanding calls than it needs and
    /// should wait for some to answer.
    TooManyInFlight = -32017, TOO_MANY_IN_FLIGHT;
    /// A second `run_turn` arrived while one was already running on that
    /// session. Not queued and not preempting: the caller decides whether to
    /// wait or to `session.interrupt` first. `data.current_turn_id` names the
    /// turn holding the session.
    SessionBusy = -32015, SESSION_BUSY;
    /// The scene requires a project (`Scene::requires_project`) but the
    /// caller asked for a session without one. Reported at `session.create`
    /// rather than at the first tool call, so a host can put the user in
    /// front of a project picker instead of a failed turn.
    ProjectRequired = -32012, PROJECT_REQUIRED;
    /// `session.taskOutput`: the session's engine knows no such task — it
    /// never ran, ended long enough ago to be forgotten, or the engine is not
    /// in memory.
    TaskNotFound = -32018, TASK_NOT_FOUND;
    /// `session.startAgent`: the session already runs as many sub-agents as
    /// `execution.max_concurrent_subagents` allows, or is as deep as
    /// delegation goes. Refused rather than queued, as the model's own
    /// `Agent` call is.
    TaskLimit = -32019, TASK_LIMIT;
    /// `mcp.addServer`/`mcp.updateServer`: the MCP policy denies this server,
    /// so nothing was written. `message` names the policy and the rule.
    McpServerDenied = -32020, MCP_SERVER_DENIED;
    /// `mcp.auth.start` / `mcp.auth.complete` / `mcp.auth.logout`: the
    /// authorization server, or the sign-in itself, failed — discovery,
    /// registration, a refused or mismatched callback, the code exchange.
    /// `message` is meant for the user.
    McpAuthFailed = -32021, MCP_AUTH_FAILED;
    /// `mcp.addServer`/`mcp.updateServer` into a project that is not trusted:
    /// the server would never start, so nothing was written.
    ProjectUntrusted = -32022, PROJECT_UNTRUSTED;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_standard_codes_are_the_json_rpc_spec_s() {
        assert_eq!(PARSE_ERROR, -32700);
        assert_eq!(INVALID_REQUEST, -32600);
        assert_eq!(METHOD_NOT_FOUND, -32601);
        assert_eq!(INVALID_PARAMS, -32602);
        assert_eq!(INTERNAL_ERROR, -32603);
    }

    #[test]
    fn codes_and_names_are_each_used_once() {
        let mut codes: Vec<i32> = ErrorCode::ALL.iter().map(|c| c.code()).collect();
        let mut names: Vec<&str> = ErrorCode::ALL.iter().map(|c| c.name()).collect();
        codes.sort_unstable();
        codes.dedup();
        names.sort_unstable();
        names.dedup();
        assert_eq!(codes.len(), ErrorCode::ALL.len());
        assert_eq!(names.len(), ErrorCode::ALL.len());
        assert_eq!(ErrorCode::SessionBusy.name(), "SESSION_BUSY");
    }
}
