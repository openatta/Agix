# grabix-01：文件工具的说明与结果都以绝对路径为准，录像换了工作目录就回放不了

类型：新能力｜优先级：中

## 现象

会话录像按位置回放模型的回复，回复里的工具参数原样执行。模型写进参数的路径，大多是从工具的说明和结果里学来的，而这些地方都是绝对路径：

- Read 的说明写「The file_path parameter must be an absolute path, not a relative path」（`crates/tools/src/prompts/coding/file_read.prompt.md`）；
  Write / Edit 的参数文档也写「The absolute path…」（`file_write.rs` 第 25 行、`file_edit.rs` 第 21 行、`file_read.rs` 第 20 行）。
  实际上三者都把相对路径按工作目录解析，相对路径本来就能用。
- Grep 结果逐行打印 `path.display()`（`grep.rs` 第 234–250 行），Glob 结果同样（`glob.rs` 第 185 行），都是绝对路径。
- Write 的「Wrote n bytes to <path>」、Edit 的「Applied … to <path>」（`file_edit.rs` 第 392 行）也带绝对路径。

于是模型在一次录制里写下 `/…/某次的临时目录/ws/tasks/x.ts`，回放时工作目录换了，同一条 Read / Edit 指向不存在的文件，后面的对话就跑偏了。
应用即使在自己的工具里只给相对路径、在提示词里要求用相对路径，也压不过这几处。

## 建议

会话级的可选项，缺省不变：

```rust
pub enum PathStyle { Absolute /* 缺省，现状 */, WorkspaceRelative }
// SessionOptions / scene 配置加一项：path_style: PathStyle
```

`WorkspaceRelative` 时：

- Read / Write / Edit 的说明改为「路径相对工作目录；也接受绝对路径」；
- Grep / Glob 结果里工作目录之下的路径打印成相对路径（工作目录之外的仍是绝对路径）；
- Write / Edit 的结果文字同样相对。

解析规则不变（相对按工作目录、绝对照用），所以只影响模型看到的文字。

## 兼容性

- 不设 `path_style` 的会话，说明与结果一字不变。
- 已有录像：结果文字只在开了这一项的会话里变；按位置回放不比对结果文字，不受影响。

## 验收

- `crates/tools` 单元测试：`WorkspaceRelative` 下，在临时工作目录里 Grep / Glob 出的路径不以工作目录开头；Write / Edit 的结果文字同样；
  缺省下与现在一致。
- 渲染出的 Read 说明在两种模式下各有对应的句子。

## 依赖

无。

## 来源

Grabix 的生成与修复会话要录下来供离线测试回放；测试的数据目录每次不同，模型照抄工具结果里的绝对路径时，录像就回放不了。
Grabix 已在自己的工具里只给相对路径，但 Read 说明的「必须是绝对路径」和 Grep / Glob 结果仍会把模型带回绝对路径。
