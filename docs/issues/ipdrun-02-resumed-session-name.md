# ipdrun-02：恢复的会话不报存着的名字，下一轮还会重新自动起名

类型：缺陷｜优先级：中

## 现象

会话池建活会话只有一个入口 `create(…, resume: bool)`（`daemon/src/session_pool.rs:1951`），不论是新建还是恢复，都把
`LiveSession` 初始化成 `name: None`、`is_first_turn: true`、`named_by_user: false`（`:2382-2394`）。于是一个
`session.rename` 过、存了标题的会话，经 `session.resume`（`:5251`）或 `session.subscribe` 的自动恢复回到内存以后：

1. `session.list` / `session.get` 对活会话取的是 `live.name`（`:4036-4050`），报 `name: null`；同一个会话冷着的时候
   读的是存着的标题（`stats.title`，`:4125-4136`），活了反而没了名字。
2. 恢复后的第一条消息被当作「第一轮」：场景开了自动起名时（`:3605-3631`），模型起的名字盖掉人起的名字（只盖在内存里，
   盘上的还是旧的，于是列表在「冷」「活」之间来回变）。
3. 顺带：自动起的名字只写进 `live.name`（`:3629`），从不落盘（`set_session_title` 只在 `rename_session` 里调，`:2614`），
   daemon 重启后自动名字就丢了。

## 建议

不加接口，修行为：

- `create` 在 `resume == true` 时从存储读标题（`stats_of(session_id).title`，`:3680`）填 `live.name`；有标题就
  `named_by_user = true`；转录里已有用户消息就 `is_first_turn = false`。
- 自动起的名字也经 `set_session_title` 落盘，但不置 `named_by_user`（人后来改名仍然优先）。若想区分「人起的」和「自动的」，
  存储的 sidecar 里加一个可选的 `title_source: "user" | "auto"`，旧文件没有这个字段就当 `user`。

## 兼容性

- 方法、参数、返回形状都不变；只是恢复后的活会话报出原来就该报的名字。
- 依赖「恢复后名字为空」的宿主（例如恢复后自己补名的）不受影响：补的名字与存着的相同，`rename` 照样成功。
- sidecar 的新字段可选，旧文件照读。

## 验收

`daemon/tests/session_lifecycle.rs` 里加：

- 建会话 → `session.rename "我起的名字"` → `session.close` → `session.resume` → `session.get` 的 `name` 是「我起的名字」；
  `session.list` 里这一行同样。
- 接着跑一轮（场景开自动起名、录像里准备好起名的回答）：名字不变（仿现有的「the name somebody typed survived the turn」用例）。
- 不 rename、让它自动起名 → 关掉 → 恢复：名字还是自动起的那个。

## 依赖

无。

## 来源

IPDrun 的宿主在 daemon 重启后恢复会话，界面上的会话名变空；现在每轮结束时查一次 `session.get`，空了就再 `rename` 一次
（`crates/agent/src/host.rs` 的 `recheck_name`）。
