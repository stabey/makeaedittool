# NSH 候选验证

## 范围

独立 workspace `spikes/nsh-eval`，不进入默认应用依赖。固定 NSH engine/core/platform/base/bundler 0.4.0；Kit 0.9.0 Fast 特性。headless 使用 `js-core`，面板使用 `js` 而禁用 platform 默认 WebView。安装但未打开面板时不创建 AppView/engine。

## 当前实现

- `nsh-eval`：有限 handshake/readRange 宿主方法；显式声明与授予方法列表；JS 小文本命令往返；脚本 throw；100ms call / 200ms load budget；64MiB heap；停止后再次创建 engine。
- `nsh-panel-eval`（`--features panel`）：同窗口 Kit Editor 与 TSX AppView；可开关面板、切换面板 appearance、打开宿主覆盖弹层，含输入框及异常/循环触发按钮。
- 面板是候选集成实验，**不是已交付编辑器插件 SDK**；headless 的范围读取使用固定实验文档，不冒充真实编辑器文档接入。

## 已观察的问题

1. 初始 serde_json 1.0.145 与 nsh-bundler 的 rolldown 1.2.13 所需 ^1.0.150 冲突；改固定已发布 1.0.154。
2. NSH capability JSON 与交接文档自定义插件清单不是相同 schema：原生字段为 `fs`（不是 `filesystem`）；`network`、`process`、`fs` 需要对象，不接受 false。查看固定版本 `nsh-core/src/access.rs` 后改为显式 host 列表；在显式声明/授予对象中未列能力默认拒绝。自定义 SDK 清单未来必须做准确转换，不能原样传入。

## 运行命令

```sh
cargo run --release --manifest-path spikes/nsh-eval/Cargo.toml --bin nsh-eval
cargo run --release --manifest-path spikes/nsh-eval/Cargo.toml --features panel --bin nsh-panel-eval
```

## 已执行结果

macOS release headless 实验通过：真实注册但未授权的方法被拒绝；handshake/readRange 返回后 JS 转换为 `HELLO!`；throw 报告预期错误；无限循环被 200ms load budget 中断；销毁后重建命令成功。

一次完整运行的观察值（非 p95、非 GUI 性能）：首次命令 9,945µs，重建命令 720µs；throw 错误到达及停止合计 1,420µs；无限循环中断及停止合计 201,100µs，停止阶段 127µs。这些是单次样本，不能作 SLA。

混合面板原版构建失败两处：rolldown helper 版本不匹配，以及 NSH platform 对 Fast 动态快捷键 API 的不兼容。已锁定 rolldown 1.2.13 helper 家族，并经评审在独立 vendor 中最小修补 `KeyBinding::load`；带补丁版本 release 构建通过。补丁、原包 SHA-256 与许可证见 [PATCHES.md](../spikes/nsh-eval/vendor/nsh-platform-gpui/PATCHES.md)，默认编辑器未改动。

## 原生窗口验证（已执行）

真实运行 `nsh-panel-eval --open-panel`，使用原生 pointer / keyboard 事件和窗口截图观察：

- TSX 面板显示 `Editor API 1`，说明实际 AppView 调用到宿主 handshake。
- 输入框正确显示并回显 Unicode 文本 `中文🙂 probe`。这是 Unicode 键盘事件注入，**不是中文 IME 预编辑/候选定位验收**。
- Kit 宿主弹层确实盖在 NSH 面板之上。
- 点击脚本异常按钮，日志报告 `Intentional panel exception`；随后 Kit 编辑器仍能输入。
- 点击无限循环按钮，日志报告 `a call into JS exceeded its budget of 100 ms`；随后 Kit 编辑器仍能输入，Cmd-Z 撤销有效。
- 关闭、重新打开面板后，宿主编辑内容保留，新面板重新显示 API 1；新面板使用暗色 appearance。循环中断后直接切换旧面板 appearance 未观察到更新，需进一步验证异常后面板状态与主题同步，不能记为完全通过。

同一进程交互后的 RSS 观察（KiB，`ps -o rss=`；非稳定峰值、非泄漏证明）：

| 阶段 | RSS KiB |
| --- | ---: |
| 异常及输入后 | 128400 |
| 循环中断及宿主输入后 | 128464 |
| 关闭面板后 | 125584 |
| 重新打开后 | 126144 |

未激活候选的另一进程曾观察到 103248 KiB，但显示与输入工作集不同，不能把两进程相减当严格 NSH 增量。

视觉证据：[混合面板](evidence/nsh-panel.png)、[宿主覆盖层](evidence/nsh-overlay.png)、[中断后继续编辑](evidence/nsh-after-loop.png)、[关闭重开](evidence/nsh-reopened.png)。

剩余门槛：真实 IME、完整 Tab 往返、宿主与面板主题同步、DPI、稳定资源分布、编辑器 SDK 接入。候选保持暂缓进入默认主线；同进程 VM 异常隔离不等同于原生崩溃隔离。
