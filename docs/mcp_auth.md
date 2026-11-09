# MCP 授权:远端 MCP server 的 OAuth

远端 MCP server(`streamable_http` / `sse`)可以要求客户端带 OAuth 访问令牌。Agix 按
[MCP Authorization 规范 2025-11-25](https://modelcontextprotocol.io/specification/2025-11-25/basic/authorization)
(OAuth 2.1)实现客户端一侧:发现授权服务器、注册客户端、在浏览器里登录、换取并刷新令牌、
带着令牌连接。浏览器在宿主那边,所以**登录由宿主驱动**:daemon 报出哪个 server 需要登录,
宿主经 RPC 发起、把用户送去授权页、(必要时)把回调交回来。

实现在 `mcp::oauth`,协议细节(发现、PKCE、`resource` 参数、issuer 校验、刷新)用 rmcp 的
`auth` 模块。

## 1. 配置

`settings.json` 的 `mcp_servers` 里,远端 server 可以有一个 `oauth` 块,**每个键都可省**:

```jsonc
"mcp_servers": {
  "docs": {
    "type": "streamable_http",
    "url": "https://mcp.example.com/mcp",
    "oauth": {
      "client_id": "agix-desktop",          // 在授权服务器上预先注册好的客户端
      "client_secret_env": "DOCS_MCP_SECRET", // 机密客户端的密钥:环境变量的名字,不是密钥本身
      "scopes": ["files:read"],              // 要请求的 scope
      "callback_port": 8765                  // 回环回调固定用这个端口(默认临时端口)
    }
  }
}
```

- **没有 `oauth` 键照样能用**:server 回 401 并给出受保护资源元数据,它就进入"需要登录"。
  `oauth` 只在要指定客户端、scope 或端口时才写。
- `client_secret_env` 必须配 `client_id`。密钥只从环境变量读,配置里写不了字面量
  (`deny_unknown_fields`:写成 `client_secret` 会被拒绝)。
- 配了固定的 `headers.Authorization` 的 server **不走 OAuth**:静态令牌照旧原样发送,401 就是
  连接失败。
- `oauth_provider` 已删除。配置里出现它,整条 server 配置被拒绝,错误信息给出 `oauth` 的写法。
- WebSocket 与 stdio server 没有 OAuth。

## 2. 状态

每个 server 有一个授权状态,在 `mcp.status` 的 `auth` 里报出:

| `auth.state` | 意思 |
|---|---|
| `none` | 没有涉及 OAuth:server 没要求,或不是远端 server |
| `required` | 需要登录:server 回了 401(没有令牌,或令牌已失效),或要求更多 scope |
| `authorizing` | 一次登录已发起,等浏览器回来 |
| `authorized` | 有令牌,连接带着它 |
| `failed` | 上一次登录失败或超时,`error` 写明原因;可以再发起 |

`auth` 还可能带 `issuer`(授权服务器)、`scopes`(已授予的;`required` 时是需要的)、
`expires_at`(RFC 3339)、`error`。**从不带令牌。**

需要登录的 server 不提供工具(`mcp.status` 里它的 `state` 是 `auth_required`,`tool_count`
为 0);模型在系统提示的 MCP 部分看到一句短说明:这个 server 要用户先登录。登录完成后只
重连这一个 server,会话在下一轮之前拿到它的工具。

## 3. 登录流程

```
宿主                              daemon                          授权服务器
 │  mcp.auth.start {server}  ─────▶ 发现、注册、PKCE
 │ ◀──── {flow_id, authorize_url, redirect_uri, expires_in}
 │  打开 authorize_url ────────────────────────────────────────▶ 用户登录、同意
 │                                  ◀──── 浏览器回到 redirect_uri?code=…&state=…
 │                                  换令牌、存、重连
 │ ◀──── daemon.event mcp_auth_completed, mcp_connected
```

**发现。** 先用 401 的 `WWW-Authenticate` 里的 `resource_metadata`,没有就探
`/.well-known/oauth-protected-resource`(RFC 9728);再按 RFC 8414 与 OpenID Connect
discovery 取授权服务器元数据。server 没有发布任何 OAuth 元数据时拒绝(不退回旧规范的
默认端点)。

**授权服务器必须支持 PKCE S256。** 元数据的 `code_challenge_methods_supported` 缺失或不含
`S256`,登录在发起前就被拒绝。

**客户端身份,按顺序取第一个可用的:**

1. `oauth.client_id`(预注册),机密客户端的密钥来自 `client_secret_env`;
2. 以前在同一个授权服务器动态注册得到、redirect URI 也对得上的客户端(回环 IP 的 redirect
   只看路径不看端口,RFC 8252 §7.3);
3. 客户端元数据文档(CIMD):宿主提供了 `client_metadata_url`,且授权服务器声明支持;
4. 动态客户端注册(RFC 7591),注册结果存下来复用;
5. 都不行:报错,请在授权服务器上注册一个客户端并写进 `oauth.client_id`。

**scope:** `mcp.auth.start` 的 `scopes` 参数 > 401 / 403 挑战里的 `scope` > 配置的
`oauth.scopes` > 受保护资源元数据的 `scopes_supported`。授权服务器支持 `offline_access` 时
自动加上,以便拿到刷新令牌。

授权请求总是带 `resource`(RFC 8707,server 的规范 URL)、S256 的 `code_challenge` 和
`state`;回调先校验 `state`,不符的回调被拒绝而流程继续等真正的那个;授权服务器在回调里
带了 `iss` 就校验它(RFC 9207)。

**回调去哪:**

- `loopback`(默认):daemon 在 `127.0.0.1` 上监听(`callback_port` 或临时端口),路径
  `/callback`,浏览器回来时它自己完成登录,并给浏览器一个"可以关掉这个窗口"的页面
  (失败时页面写明原因)。
- `external`:宿主给出自己的 `redirect_uri`(`https`、回环地址上的 `http`,或带 `.` 的私有
  scheme,如 `com.example.app:/oauth`),自己接住回调,把完整的回调 URL 经
  `mcp.auth.complete` 交回。

可以同时有多个登录在进行;同一个 server 再发起一次会取消它前一次。登录 10 分钟内没完成就
过期,发 `mcp_auth_failed`,状态变 `failed`。

### 令牌的生命周期

- 令牌快到期(剩不到 30 秒)时在用之前刷新;授权服务器轮换刷新令牌时存新的。同一个 server
  的刷新在进程内串行,两个连接不会拿同一个旧刷新令牌各刷一次。
- **只有两种情况会丢掉令牌**:刷新被授权服务器以 `invalid_grant` 拒绝;或令牌被 server 以
  401 拒绝且刷新也救不回来。这时删除令牌(**保留**客户端注册),状态回到 `required`,
  发 `mcp_auth_required`,server 从所有连接里摘下。
- 网络错误、5xx、超时、授权服务器暂时连不上:**令牌不动**,只是这次连接失败,照常重试。
- 403 `insufficient_scope`:令牌对别的请求仍然有效,server **保持连接**;状态变 `required`,
  `scopes` 是已授予的与新要求的并集,发 `mcp_auth_required`。再 `mcp.auth.start` 就按这个
  并集请求。

### 登出

`mcp.auth.logout` 在授权服务器提供 `revocation_endpoint` 时吊销刷新令牌与访问令牌
(RFC 7009),然后删除令牌、断开 server,状态回到 `required`。客户端注册保留,下次登录不必
重新注册。

## 4. RPC 与通知

方法(参数与返回见 `daemon_rpc_protocol.md` §6.5):

| 方法 | 作用 |
|---|---|
| `mcp.auth.start` | 发起登录,返回 `{flow_id, authorize_url, redirect_uri, expires_in}` |
| `mcp.auth.complete` | external 流程:交回回调 URL,换令牌、重连,返回 `{server, scopes}` |
| `mcp.auth.cancel` | 放弃一次登录,server 回到发起前的状态 |
| `mcp.auth.logout` | 吊销、删除令牌、断开 |

`daemon.event`:

| `kind` | 载荷 | 什么时候 |
|---|---|---|
| `mcp_auth_required` | `{server, scopes}` | server 需要登录(含需要更多 scope) |
| `mcp_auth_completed` | `{server}` | 登录完成,随后是这个 server 的 `mcp_connected` |
| `mcp_auth_failed` | `{server, error}` | 登录失败或过期 |

错误码:请求本身不对(没有这个 server、server 不用 OAuth、流程不存在或已结束、redirect
不合法)是 `INVALID_PARAMS`;授权服务器或登录本身失败是 `MCP_AUTH_FAILED`(-32021);MCP
策略拒绝的 server 是 `MCP_SERVER_DENIED`——被拒的 server 根本走不到 OAuth。

## 5. 凭据存储

默认存在用户数据目录的 `mcp/oauth/` 下:每个 server 一个文件,文件名是规范化 server URL 的
SHA-256,权限 0600(目录 0700),写入走临时文件 + 原子改名。存的是一条
`mcp::oauth::McpCredentials`:server URL、客户端注册(动态注册得到的 `client_id` /
`client_secret`、redirect URI、issuer)、令牌(访问令牌、刷新令牌、到期时间、scope、令牌所属的
客户端)、issuer、授权服务器元数据(发现失败时退回它,登出时从中找吊销端点)。

**以规范化 URL 为键**:去掉 fragment 与 userinfo,主机名小写、默认端口省略,只有主机时去掉
末尾的 `/`。所以在 settings 里给 server 改名,登录还在。

### 自己的存储

嵌入方可以换掉存储(如放进系统钥匙串),实现一个小 trait 并经 `Assembly` 传入:

```rust
#[async_trait::async_trait]
pub trait McpCredentialStore: Send + Sync {
    async fn load(&self, server_url: &str) -> std::io::Result<Option<McpCredentials>>;
    async fn save(&self, server_url: &str, credentials: &McpCredentials) -> std::io::Result<()>;
    async fn delete(&self, server_url: &str) -> std::io::Result<()>;
}

let pool = assembly::assemble::pool(&config, scene, Assembly {
    mcp_credential_store: Some(Arc::new(MyKeychain::new())),
    mcp_oauth_client: Some(McpOAuthClient {
        client_name: "My App".into(),
        client_metadata_url: Some("https://myapp.example/oauth/client.json".into()),
    }),
    ..Default::default()
}).await?;
```

- `McpCredentials` 实现了 `Serialize` / `Deserialize`,整条记录 `serde_json::to_string` 之后
  可以当一个机密存;它的 `Debug` 不显示任何机密。
- 没存过的 URL,`load` 返回 `Ok(None)`。**后端暂时不可用要返回错误,不要返回 `None`**:
  `None` 的意思是"未登录",会让用户重新登录。
- daemon 对同一条记录的读改写在进程内串行;跨进程共享同一份存储时,协调归存储自己。
- `mcp_oauth_client` 决定动态注册时报给授权服务器的名字(用户在同意页上看到),以及 CIMD
  用的元数据文档 URL;省略时名字是 `Agix`,只用动态注册。

## 6. 安全

- **SSRF。** 发现与授权服务器的请求只能到公网地址,外加 MCP server 自己那一类地址:server
  在回环上,授权服务器也可以在回环上;server 在私网,也可以在私网。链路本地(含云元数据
  `169.254.169.254`)、组播、未指定地址永远不行。IP 字面量和 DNS 解析结果都检查。配了
  `HTTP(S)_PROXY` 时由代理解析域名,只能按主机名检查;回环上的 server 的 OAuth 请求不走代理。
- **redirect URI** 只有 daemon 自己的 `127.0.0.1` 回环地址,或宿主给的(见 §3)。
- **令牌只出现在凭据存储里**:不进日志、遥测、会话录像与 transcript、`daemon.event`、任何 RPC
  响应。只以 `Authorization: Bearer` 头发送。
- 机密客户端的密钥只从环境变量读;动态注册得到的密钥存在凭据记录里。

## 7. 限制

- 不支持 DPoP 与 mTLS 绑定的令牌;只发 Bearer。
- 不做 RFC 9207 之外的回调来源校验;回环监听只接受 `GET /callback`。
- 刷新的跨进程协调不在这里:多个 daemon 共享同一份存储且授权服务器轮换刷新令牌时,可能有一个
  刷新失败并要求重新登录。
- WebSocket server 连不上退回 streamable HTTP 时不带 OAuth。
