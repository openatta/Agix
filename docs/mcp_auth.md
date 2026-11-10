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
      "issuer": "https://login.example.com", // 这个客户端注册在哪个授权服务器(它的 issuer)
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
- **密钥只交给它所属的授权服务器。** 授权服务器是 MCP server 自己在受保护资源元数据里指的,
  server 换一个指向就能把密钥引到别处。所以配了 `client_secret_env`,还要有下面之一:
  `issuer` 写明这个客户端注册在哪个授权服务器(与发现到的 issuer 去掉末尾 `/` 后完全相等),
  或 MCP 策略写了 `allowed_authorization_servers`(`ARCHITECTURE.md` §6.4)。issuer 不符时
  `mcp.auth.start` 报 `MCP_SERVER_DENIED`,两样都没有时报 `INVALID_PARAMS`,密钥都不发出;
  刷新、吊销与无人值守的令牌请求同样只在 issuer 相符时带上它。动态注册得到的密钥只发给当初
  注册它的那个 issuer。
- **密钥也不经子进程的环境流出。** `client_secret_env`、`private_key_jwt.key_env` 与企业 IdP 的
  `client_secret_env` 指向的变量,以及 server 的 `url` / `headers` / `env` / `command` / `args` 里
  `$VAR` 引用的变量,daemon 加载设置、启用插件、装配 MCP 策略时就记下,任何子进程都不继承:
  `Bash`、`Monitor`、command hook、引擎自己跑的 git、`pdftotext`,以及别的 MCP stdio server——模型一句
  `printenv` 读不到(`ARCHITECTURE.md` §5 第 5 条)。stdio server 自己 `env` 里写的照给,继承来的
  环境里没有别的 server 的密钥。确实要让命令也拿到某个变量的,写进 `sandbox.subprocess_env.allow`。
- **项目层的 server 读不了 daemon 的环境,也用不了 daemon 的身份。** 项目
  `.agix/settings.json` / `settings.local.json` 里声明的 server(`mcp.addServer` 写的也是这一层)
  不能用 `client_secret_env`、`private_key_jwt.key_env`(读 daemon 的环境)、
  `private_key_jwt.key_file`(读本机上的私钥文件)和 `"grant": "enterprise"`(花的是宿主交来的
  企业身份),写了就不连接(`mcp_connect_failed`;`mcp.addServer` 报
  `INVALID_PARAMS`);它的 `url` / `headers` 里的 `$VAR` 也不从 daemon 的环境展开
  (`ARCHITECTURE.md` §6.4)。机密客户端放在全局层或场景层声明。项目没被信任时,它自己的
  server 根本不启动,也就不会走到登录(`mcp.status` 里是 `untrusted`,`ARCHITECTURE.md` §6.6)。
- 配了固定的 `headers.Authorization` 的 server **不走 OAuth**:静态令牌照旧原样发送,401 就是
  连接失败。
- `oauth_provider` 已删除。配置里出现它,整条 server 配置被拒绝,错误信息给出 `oauth` 的写法。
- WebSocket 与 stdio server 没有 OAuth。
- `grant` 选令牌从哪来:`authorization_code`(默认,用户在浏览器里登录,§3)、
  `client_credentials`、`enterprise`(无人值守,§4)。

## 2. 状态

每个 server 有一个授权状态,在 `mcp.status` 的 `auth` 里报出:

| `auth.state` | 意思 |
|---|---|
| `none` | 没有涉及 OAuth:server 没要求,或不是远端 server |
| `required` | 需要登录:server 回了 401(没有令牌,或令牌已失效),或要求更多 scope;`enterprise` server 在等企业身份(§4) |
| `authorizing` | 一次登录已发起,等浏览器回来 |
| `authorized` | 有令牌,连接带着它 |
| `failed` | 上一次登录失败或超时,或无人值守的 server 取不到令牌;`error` 写明原因 |

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

可以同时有多个登录在进行;同一个 server 再发起一次会取消它前一次,并先关掉前一次的回环监听,
所以固定的 `callback_port` 可以反复发起;取消、过期、登出同样立刻释放端口。登录 10 分钟内没完成就
过期,发 `mcp_auth_failed`,状态变 `failed`。

### 令牌的生命周期

- 令牌快到期(剩不到 30 秒)时在用之前刷新;授权服务器轮换刷新令牌时存新的。同一个 server
  的刷新串行:进程内一把锁,默认的文件存储再对这条记录的锁文件加 `flock`,所以共用一个
  `mcp/oauth/` 的几个 daemon / CLI 也是一个刷完另一个才开始。后一个拿到锁之后重读记录,用的是
  前一个刚换来的刷新令牌,不会拿已经作废的旧令牌去刷。
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
重新注册。登出全程持有刷新用的那把锁:正在进行的刷新先做完,登出吊销的是它换来的令牌;
刷新写回之前也会看一眼,记录里的令牌已经不是它开始时那一份(被登出清掉,或被别处换掉),
就不写回。

## 4. 无人值守:client credentials 与企业托管授权

没人坐在浏览器前的 server(定时任务、服务端部署)用这两种 `grant`。令牌在用之前检查:剩不到
60 秒就重新取(寿命本来就不到 60 秒的令牌,每次用之前都取一个新的,取到的就用,不再因为
"快到期"被拒);server 以 401 拒绝时重新取一次并重试那次调用。都不需要 `mcp.auth.start`
(对它们调会得到 `INVALID_PARAMS`)。

### `client_credentials`

daemon 以自己的身份取令牌(OAuth 2.0 client credentials,MCP 的 SEP-1046),请求带
`resource` 与配置的 `scopes`:

```jsonc
"oauth": { "grant": "client_credentials", "client_id": "agix-batch",
           "client_secret_env": "BATCH_MCP_SECRET",       // client_secret_basic / _post,按授权服务器支持的
           "issuer": "https://login.example.com" }        // 密钥只交给这个授权服务器(§1)
"oauth": { "grant": "client_credentials", "client_id": "agix-batch",
           "private_key_jwt": { "key_file": "/etc/agix/mcp-client.pem",   // 或 "key_env": "VAR"
                                "alg": "ES256" } }        // RS256(默认) RS384 RS512 ES256 ES384
```

`client_id` 必填,`client_secret_env` 与 `private_key_jwt` 恰好一个。`private_key_jwt`
(RFC 7523)每次请求现签一个 5 分钟有效的断言(`iss` = `sub` = `client_id`,`aud` = 授权服务器
issuer);令牌端点必须是 HTTPS,只有 server 与授权服务器都在回环上时才允许 HTTP。

### `enterprise`:企业托管授权(ID-JAG)

用户已经登录了企业 IdP,宿主手里有他的 ID token(或 IdP 的刷新令牌)。daemon 拿它到 IdP 做
token exchange(RFC 8693),换一个发给 MCP server 的授权服务器、绑定这个 server 的身份断言
授权(ID-JAG),再到那个授权服务器以 JWT bearer 授权(RFC 7523)换访问令牌。用户不必为每个
MCP server 各登录一次,企业 IdP 决定谁能用哪个 server。

**企业 IdP** 由部署方给,不在用户的 settings 里——宿主经 `Assembly::mcp_enterprise_idp`,
或 MCP 策略文件的 `enterprise_idp`(两者都有时以宿主的为准):

```jsonc
"enterprise_idp": {
  "issuer": "https://login.corp.example",
  "token_endpoint": "https://login.corp.example/oauth2/token",
  "client_id": "agix",                         // 这个客户端在 IdP 上的注册
  "client_secret_env": "AGIX_IDP_SECRET",      // 可省:公共客户端
  "client_auth": "client_secret_basic"         // 可省:有密钥就 basic,没有就 none;或 client_secret_post
}
```

**每个 server:**

```jsonc
"oauth": { "grant": "enterprise",
           "client_id": "agix-mcp",              // 这个客户端在 server 的授权服务器上的注册
           "client_secret_env": "DOCS_AS_SECRET", // 可省;写了就要有 issuer(§1)
           "issuer": "https://as.docs.example",
           "scopes": ["files:read"] }
```

**身份**由宿主经 `mcp.auth.setIdentity {id_token}` 或 `{refresh_token}` 交给 daemon,只放在
内存里,daemon 重启要再给一次。在拿到身份之前,这些 server 处于 `auth_required`,`auth.error`
说明在等身份,并发 `mcp_auth_required`;拿到后它们随即换令牌、连接。令牌到期时用同一个身份
再换。IdP 拒绝这个身份(`invalid_grant`,如 ID token 过期)时,server 回到 `auth_required`,
宿主要换一个新的身份。

## 5. RPC 与通知

方法(参数与返回见 `daemon_rpc_protocol.md` §6.5):

| 方法 | 作用 |
|---|---|
| `mcp.auth.start` | 发起登录,返回 `{flow_id, authorize_url, redirect_uri, expires_in}` |
| `mcp.auth.complete` | external 流程:交回回调 URL,换令牌、重连,返回 `{server, scopes}` |
| `mcp.auth.cancel` | 放弃一次登录,server 回到发起前的状态 |
| `mcp.auth.logout` | 吊销、删除令牌、断开 |
| `mcp.auth.setIdentity` | 给出企业身份,重连等着它的 `enterprise` server |

`daemon.event`:

| `kind` | 载荷 | 什么时候 |
|---|---|---|
| `mcp_auth_required` | `{server, scopes}` | server 需要登录(含需要更多 scope、`enterprise` server 在等身份) |
| `mcp_auth_completed` | `{server}` | 登录完成,随后是这个 server 的 `mcp_connected` |
| `mcp_auth_failed` | `{server, error}` | 登录失败或过期 |

错误码:请求本身不对(没有这个 server、server 不用 OAuth、流程不存在或已结束、redirect
不合法)是 `INVALID_PARAMS`;授权服务器或登录本身失败是 `MCP_AUTH_FAILED`(-32021);MCP
策略拒绝的 server、不在 `allowed_authorization_servers` 里的授权服务器是
`MCP_SERVER_DENIED`——被拒的 server 根本走不到 OAuth。

## 6. 凭据存储

默认存在用户数据目录的 `mcp/oauth/` 下:每个 server 一个文件,文件名是规范化 server URL 的
SHA-256,权限 0600(目录 0700),写入走临时文件 + 原子改名。旁边的 `<哈希>.lock` 是刷新与
登出时 `flock` 的那个文件(记录本身每次写都换一个 inode,锁不住)。存的是一条
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
    // 可省:返回的值活着期间,别的进程对同一条记录的刷新与登出要等着
    async fn lock(&self, server_url: &str) -> std::io::Result<Option<Box<dyn Send>>> {
        Ok(None)
    }
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
- daemon 对同一条记录的读改写在进程内串行。刷新与登出还会先拿 `lock`:默认的文件存储
  对 `<哈希>.lock` 加 `flock`,自己的存储要跨进程协调就实现它,不实现就只有进程内的串行。
- `mcp_oauth_client` 决定动态注册时报给授权服务器的名字(用户在同意页上看到),以及 CIMD
  用的元数据文档 URL;省略时名字是 `Agix`,只用动态注册。

## 7. 安全

- **SSRF。** 发现与授权服务器的请求只能到公网地址,外加 MCP server 自己那一类地址:server
  在回环上,授权服务器也可以在回环上;server 在私网,也可以在私网。链路本地(含云元数据
  `169.254.169.254`)、组播、未指定地址永远不行。IP 字面量和 DNS 解析结果都检查,带着内部
  IPv4 的各种 IPv6 写法(映射、SIIT `::ffff:0:a.b.c.d`、NAT64 含本地用的 `64:ff9b:1::/48`、
  6to4、Teredo)按它带着的 IPv4 算,判定与模型发起的请求共用 `is_internal_address`。
  **配了 `HTTP(S)_PROXY` 时域名由代理替我们解析**,daemon 看不到解析结果,只能按主机名和
  IP 字面量检查;要靠地址过滤挡住内网,就别让 MCP 的流量走一个能进内网的代理。回环上的
  server 的 OAuth 请求不走代理。
- **MCP 连接本身。** `streamable_http` / `sse` 的请求只跟同源(scheme、主机、端口都相同)的
  重定向,跨源的 3xx 原样交回、不跟,配置的 `headers` 不会被带到别的主机。server 不在回环上时,
  它的主机名解析出的回环、链路本地、组播等地址一律不连(私网照常可以,企业内网的 server 常在
  那里);要连本机的 server,URL 里就写回环地址。
- **URL 里的凭据不外露。** `mcp.status`、`mcp_connect_failed` 与错误信息里出现的 URL 去掉
  userinfo、query 与 fragment(`https://u:p@h/mcp?key=…` 报成 `https://h/mcp`)。
- **只走 HTTPS。** 元数据发现、issuer、注册、令牌、吊销端点必须是 `https`;只有在回环地址上的
  授权服务器可以用 `http`(OAuth 2.1 §1.5)。私网的 `http` 授权服务器同样拒绝,企业 IdP 的端点也一样。
  例外只有一处:MCP server 本身在回环上(本地开发)时,发现请求不强制 `https`,但取回的 issuer
  与各端点仍按上面检查。
- **交给宿主打开的 `authorize_url`** 只会是 `https`,或回环地址上的 `http`;授权服务器给出
  `file:`、`javascript:` 或别的应用协议的授权端点时,`mcp.auth.start` 以 `MCP_AUTH_FAILED` 失败,
  不注册客户端、不交出 URL。
- **redirect URI** 只有 daemon 自己的 `127.0.0.1` 回环地址,或宿主给的(见 §3)。
- **令牌只出现在凭据存储里**:不进日志、遥测、会话录像与 transcript、`daemon.event`、任何 RPC
  响应。只以 `Authorization: Bearer` 头发送。
- 机密客户端的密钥只从环境变量读,这些变量不交给任何子进程(§1);动态注册得到的密钥存在凭据
  记录里;签断言的私钥从文件或环境变量读,不进凭据记录。
- **授权服务器白名单。** MCP 策略(`ARCHITECTURE.md` §6.4)可以写
  `"allowed_authorization_servers": ["https://login.corp.example", "https://corp.okta.com/oauth2/default"]`:
  对 issuer(规整成 `scheme://host[:port][/path]`、末尾不带 `/`)按 URL 模式拆开匹配(`ARCHITECTURE.md`
  §6.1 第 5 条):主机里的 `*` 只跨 `.`、到不了路径,路径里的 `*` 不跨 `/`。写具体的
  租户:`https://*.okta.com/**` 这样的通配放进的是**任何人**在 Okta 上开的租户,包括攻击者自己
  注册的那个。不匹配的
  授权服务器在**任何跳转、注册、令牌请求之前**被拒绝;写了这一项而授权服务器元数据没有
  `issuer`,同样拒绝。不写 = 任何授权服务器都行。
- 企业 IdP 的端点是部署方配置的,不受上面"只能到公网"的地址限制;server 的授权服务器仍然受。

## 8. 限制

- 不支持 DPoP 与 mTLS 绑定的令牌;只发 Bearer。
- 不做 RFC 9207 之外的回调来源校验;回环监听只接受 `GET /callback`。
- 跨进程的刷新是排队,不是合并:后拿到锁的进程用新的刷新令牌再刷一次,而不是直接用前一个
  刚换来的访问令牌(rmcp 拿到锁后总会发令牌请求)。令牌不会因此作废,只是多一次请求。
- WebSocket server 连不上退回 streamable HTTP 时不带 OAuth。
- 企业托管授权:只检查 ID-JAG 的结构与绑定(issuer、audience、client、resource、scope、有效期),
  **不验签**——它直接来自受信的 IdP 令牌端点,签名由 server 的授权服务器验证;不支持 Rich
  Authorization Requests(`authorization_details`);ID token 的新鲜度归宿主,daemon 不替它刷新;
  身份只在内存里。
- `client_credentials` 的令牌请求在一个自己的线程与运行时上完成(rmcp 的这段 future 不是
  `Send`);一个令牌有效期里只发生一次,不影响别的连接。
