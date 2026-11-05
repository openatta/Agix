# 部署 agixd

`agixd` 是一个常驻进程:会话、子 agent 与它们启动的命令都活在里面。这份文档讲
怎么构建它、它崩溃时做什么、以及怎么让 supervisor 把它拉起来。

## 构建

| profile | 命令 | 失败语义 |
|---|---|---|
| `release` | `cargo build --release -p daemon --bin agixd` | panic 时展开;任务边界内的 panic 只结束那个任务 |
| `release-small` | `cargo build --profile release-small -p daemon --bin agixd` | panic 时直接中止进程 |

两者都用 fat LTO。`release-small` 另加 `opt-level = "s"` 与 `panic = "abort"`,产物约为
`release` 的一半,daemon 自身每轮的耗时多一成左右。体积更要紧时可以再加
`--config 'profile.release-small.opt-level="z"'`,再小三分之一,耗时多三分之二。

中止意味着一个 bug 让整个进程退出,而不是带着可能不一致的状态继续跑——前提是
有人把它重新拉起来,也就是下面的 supervisor。

## 崩溃时发生什么

命令(`Bash` 等)各自领一个进程组,不会随 daemon 退出而结束。因此 daemon 在两处
替它们收尾:

1. **panic 时**(`release-small`):panic hook 先对所有在跑命令的进程组发
   `SIGKILL`,再写崩溃记录,然后中止。`release` 下 panic 可能只结束一个任务,
   hook 只写记录,不动命令。
2. **下次启动时**:每个实例把在跑命令的进程组记在
   `~/.agix/daemon/groups/<instance>.json`。被 `SIGKILL`、OOM 或断电带走的 daemon
   来不及执行 hook;同名实例下次启动、拿到锁之后,会先停掉这份记录里仍在运行的
   进程组(`SIGTERM`,500 ms 后 `SIGKILL`),日志里有一条
   `stopped process groups a previous run of this instance left running`。
   记录的主人仍在运行时不动它。

崩溃记录写在 `~/.agix/daemon/crashes/<unix 秒>-<pid>.txt`:版本、线程、panic 位置
与消息、被停掉的进程组、backtrace。`release` 与 `release-small` 都去掉了符号,
backtrace 只有地址;要可读的 backtrace,用带符号的构建复现。

`AGIX_CONFIG_HOME` 设了的话,上面的 `~/.agix` 换成它。

会话的历史在每次写入时落盘,daemon 重启后照常 `session.resume`。崩溃时正在进行的
那一轮不会续跑:它的工具调用已经被停掉,客户端看到连接断开,重新发起即可。

## supervisor

daemon 自己不重启自己。用系统的 supervisor,并让它在非正常退出时重启。

### launchd(macOS)

`~/Library/LaunchAgents/dev.agix.agixd.plist`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>            <string>dev.agix.agixd</string>
  <key>ProgramArguments</key>
  <array>
    <string>/usr/local/bin/agixd</string>
    <string>--scene</string> <string>coding</string>
  </array>
  <key>KeepAlive</key>
  <dict><key>SuccessfulExit</key><false/></dict>
  <key>ThrottleInterval</key> <integer>5</integer>
  <key>StandardErrorPath</key><string>/tmp/agixd.err.log</string>
  <key>EnvironmentVariables</key>
  <dict><key>RUST_LOG</key><string>info</string></dict>
</dict>
</plist>
```

```sh
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/dev.agix.agixd.plist
```

`SuccessfulExit = false`:正常退出(`daemon.shutdown`、Ctrl-C)不重启,崩溃才重启。

### systemd(Linux,用户级)

`~/.config/systemd/user/agixd.service`:

```ini
[Unit]
Description=Agix daemon

[Service]
ExecStart=/usr/local/bin/agixd --scene coding
Restart=on-failure
RestartSec=2
# 默认的 control-group 模式会在 daemon 退出时连带杀掉它启动的命令;
# 这里只停主进程,命令由 daemon 自己收尾。
KillMode=process
Environment=RUST_LOG=info

[Install]
WantedBy=default.target
```

```sh
systemctl --user daemon-reload
systemctl --user enable --now agixd
```

`KillMode=process` 是有意的:`daemon.shutdown` 与 `SIGTERM` 走正常关闭,daemon
按会话设置决定哪些后台命令停、哪些留;交给 systemd 整组杀掉会抹掉这个区别。
