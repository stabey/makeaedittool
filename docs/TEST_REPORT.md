# 验证记录

## 初始环境探测（已执行）

- `rustc --version && cargo --version`：均为 1.99.1。
- `xcode-select -p`：完整 Xcode 安装可见。
- `xcrun -f metal`：Metal Toolchain 编译器可见。
- `sysctl hw.memsize hw.model`：34,359,738,368 bytes，Mac16,10。
- `df -h .`：约 1.4 TiB 可用，允许生成 1/10 GiB 非稀疏实验数据。
- `osascript -e 'tell application "System Events" to get UI elements enabled'`：true；仍需实际 GUI 运行证明交互。
- `prlctl list -a`：Windows 11 guest 正在运行；guest `PROCESSOR_ARCHITECTURE` 返回 ARM64。未执行 Windows x64 产品验收。
- `cargo info gpui-kit@0.9.0`、`cargo info nsh-platform-gpui@0.4.0`、`cargo info nsh-engine@0.4.0`：发布包可获取。
- `cargo fetch --manifest-path spikes/nsh-eval/Cargo.toml`：首轮依赖解析与下载成功；不代表构建成功。
- 项目无配置的 Rust LSP，本轮不能依赖语言服务器提供引用/诊断。

## 集成后验收协议

本节是待执行矩阵，不是通过记录。

1. 普通编辑：原生窗口中文/Emoji、输入、选择、撤销重做；程序化 edits 的版本/原子性；未提交 IME 不落盘。
2. 保存：UTF-8/BOM/LF/CRLF、编辑期间保存捕获版本、撤销至保存点、错误不覆盖原文件、恢复库故障仍可抢救保存。
3. 恢复：多个未命名标签、正常退出/重启、已确认事务后强杀、原路径不存在、损坏单草稿隔离、关闭与丢弃生命周期。
4. 大文件：1/10 GiB 非稀疏日志首范围、128 MiB 单行、密集短行、跨块 UTF-8/CRLF、超过 32 位偏移、多个文件共享预算、源变化失效。
5. 搜索：块大小改变不改变非重叠结果、空查询、限额、满结果队列取消、关闭文档停止采纳旧批次。
6. NSH：独立 headless 调用、显式权限、异常、限时循环、重启；Fast 混合窗口 AppView 焦点、IME、弹层和资源单列。

运行证据与未通过项将在实际执行后追加；脚本模拟不替代原生 IME 人工候选窗口验证，进程强杀不等同于掉电验证。

## 服务层实际执行

- 首轮 `cargo test -p editor-services`：23 通过、1 失败。100 标签测试生产者未处理有界队列 Busy；修复为保留提交并重试，必须收到全部100文档 durable 确认，不扩大队列、不丢文档。单场景复测通过。
- 独立评审后，`cargo test -p editor-services`：**36 个测试通过**。随后 release 服务可执行文件构建成功。
- `target/release/editor-services smoke .bench-data`：修复后实际运行通过 `open_bom_crlf,delta_recovery,save_temp_fault,atomic_save,cross_block_nonoverlap,discard_guard`。
- `recovery-bench .bench-data/recovery-cost 4MiB 20`：实际完成20个增量提交；包括 worker 重放一致性比较与 SQLite FULL 提交。延迟数据归性能报告，不能外推掉电安全。

服务评审与数据安全复查已经关闭本轮阻塞项：公平动态索引预算、最新查询取代旧查询、唯一源代际与截断失效、有界解码 CRLF、完整扫描与实际读取字节分开、非阻塞 I/O 配额；以及没有布局记录的 durable 草稿仍可发现、损坏单行元数据不阻断其他草稿、错误但等长的 delta 不得替代权威快照。

元数据损坏的草稿保留在数据库并报告 warning，但正常 hydration 不提供其抢救导出；此限制不等同于其他草稿恢复失败。独立评审本身没有执行测试，运行结果来自以上命令。

## NSH 独立原生实验

release headless 与带最小兼容补丁的 Kit Fast/AppView 均实际构建运行；宿主覆盖层、Unicode 事件输入、脚本异常、限时循环后继续宿主编辑、撤销、关闭重开有原生截图。完整证据和未通过门槛见 [NSH_EVALUATION](NSH_EVALUATION.md)。这些结果不冒充默认编辑器功能验收。

## 原生应用集成构建

`cargo fmt --all`、`cargo test --workspace`、`cargo build --release -p editor` 已实际执行成功。首次集成为20个编辑器测试+36个服务测试；加入原生运行发现问题的回归后，最终为**23个编辑器测试 + 36个服务测试 = 59通过**。首轮证据见 [workspace-tests.json](evidence/workspace-tests.json)，最终见 [workspace-final.txt](evidence/workspace-final.txt)。

首次集成暴露Rust2024保留字、opaque return生命周期捕获、AsyncApp借用及实际恢复API不匹配，均修复后重新执行。没有通过删掉行为断言或放宽恢复确认门槛来绕过失败。

UI实现评审修复了密集行虚拟化、后台可取消普通搜索、关闭/退出屏障、超预算恢复提示、准入回收和冲突重载。重载丢弃旧草稿必须创建新DocumentId，不能重用永久tombstone；退出close ACK超时必须失败，不能当作已durable。编译和静态评审不替代下列原生运行验证。

## 原生窗口已观察行为

独立runtime-verify启动真实AppKit/Metal窗口，通过CoreGraphics键盘/指针事件完成中文与Emoji输入、全选、单次撤销、重做、字面量查询，并正常退出后重启恢复相同未保存草稿（rev 7 durable）。这是实际窗口行为；没有把Unicode注入当作IME候选与预编辑验收。证据：[恢复后的编辑区](evidence/native-manual-reopened.png)、[搜索](evidence/native-manual-search.png)、[撤销](evidence/native-manual-undo.png)。

大文件真实窗口已显示64 GiB日志首片、128 MiB单行片段、512 MiB密集行虚拟列表；分别见 [64 GiB](evidence/native-largefile-sixtyfour.png)、[长行](evidence/native-largefile-line.png)、[密集行](evidence/native-largefile-dense.png)。截图最初命名错误，经对照活动标签及总字节数纠正。多标签窗口不是单独64 GiB首屏延迟实验；脚本500ms等待也不计为切换耗时。

首次自动smoke发现两个集成缺陷：macOS `cx.quit()`直接终止进程，放在`application.run()`之后的报告代码不可达；空白未命名标签没有checkpoint，却被错误送入恢复内容查询。失败证据保留于 [native-full-failure.json](evidence/native-full-failure.json)。两项均已修复并通过完整原生smoke，不能用首次exit 0当作通过。100标签修复后三轮均输出真实报告：100标签、1个hydrated编辑器、69,632内容字节、durable shutdown确认，详见性能报告。

## 最终完整原生Smoke

```sh
cargo test --workspace
cargo build --release -p editor
target/release/editor --data-dir .smoke-data/native-full-atomic --smoke=full --smoke-report docs/evidence/native-full.json
```

实际exit 0，`passed=true`、`in_app_checks_passed=true`、`shutdown_durable_ack_observed=true`。报告在原始SQLite worker shutdown确认、重新打开数据库验证及验证worker shutdown确认之后、AppKit退出之前写出。

| 场景 | 结果 |
|---|---|
| 中文/Emoji字面量查询；两个扫描内核结果一致 | 通过；分页搜索72匹配 |
| 多位置原子编辑一次撤销；拒绝旧版本 | 通过 |
| 同一UI更新内连续程序化编辑；NoOp不推进版本 | 通过 |
| 等长未交付原生编辑被拒绝；事件交付后重试 | 通过 |
| 替换后查询基于新快照，维持Done而非Invalidated | 通过 |
| BOM/CRLF保存、关闭后重开 | 通过 |
| 丢弃旧草稿并重载到新DocumentId；继续编辑durable | 通过 |
| shutdown后新连接读取清单与草稿内容 | 通过 |
| 空白未命名标签无checkpoint时恢复 | 通过 |
| 1 MiB普通文档128次输入/删除/撤销提交，恢复开启 | 通过；延迟见性能报告 |

完整结果：[native-full.json](evidence/native-full.json)。中间还修复了smoke相对路径与产品canonical路径比较导致的假超时，保留 [native-full-hydration-failure.json](evidence/native-full-hydration-failure.json)；未改变文件路由阈值或放宽超时掩盖问题。

独立runtime-verify在最终二进制上再次执行完整smoke及同数据重启，均通过：[独立完整报告](evidence/native-final-verifier.json)、[同数据启动报告](evidence/native-final-restart.json)。重启保留4个标签，只hydrate活动的空白未命名文档，内容0字节且无恢复错误。

实际按钮替换另有前后视觉证据：[替换字段已填写](evidence/native-manual-replace-field.png) → [点击Replace后3匹配降为2](evidence/native-manual-replace-done.png) → [退出重启保留替换内容](evidence/native-manual-replace-reopened.png)。早先`native-manual-replace.png`中字段为空，属于自动化点错位置后直接编辑正文，正确触发了查询失效；不将那张图当作Replace按钮验收。

最终 `cargo fmt --all -- --check` 通过；[被测二进制与锁文件标识](evidence/build-identity.txt)。单位测试数量59与smoke布尔场景是不同计数，不把它们混为“59个原生交互场景”。

## 日常编辑功能补齐

`cargo test --locked --workspace`：**38个编辑器测试 + 19个服务单元测试 + 35个服务集成测试 = 92通过**；release构建成功。原始输出：[usability-workspace.txt](evidence/usability-workspace.txt)。测试覆盖有界偏好存储、损坏配置保留、保存格式基线、元数据入队顺序及CRLF行编辑边界。

集成中实际发现并修复：

- 缺少`Focusable` trait导入导致编译失败。
- 把元素上下文`Input mode=editor`误用为快捷键谓词，启动时panic；改为`Input && mode == editor`后真实启动通过。
- 评审发现从Go To打开/关闭面板可能恢复到已卸载输入框，以及面板转Find时残留遮罩；修复后加入原生按键派发回归。
- 草稿批处理与即时布局更新可能逆序覆盖较新的语言/格式元数据；以有界、单调接纳序号修复，事务成功后才推进水位。确定性调度回归与无中间flush的live-worker回归通过，独立复审关闭阻塞项。

`--smoke=usability`在真实AppKit/Metal窗口执行，不是headless服务测试。报告：[usability-native.json](evidence/usability-native.json)，按键扩展输出：[usability-keymap-output.txt](evidence/usability-keymap-output.txt)。

| 已执行场景 | 结果 |
|---|---|
| 面板Select All/Undo作用于正文而非查询框 | 通过 |
| GPUI按键派发：Go To → 面板 → Escape；面板 → Find | 正文/Find焦点正确，无残留面板 |
| Find内复制行快捷键不改正文；正文内复制行后一次撤销 | 通过 |
| GPUI派发Alt+Z切换折行且不插入正文 | 通过；不是OS Option键输入验收 |
| 显示/语言设置不dirty，未激活编辑器惰性应用 | 通过 |
| 快速切换过滤不hydrate未加载标签 | 通过 |
| CRLF、末行无换行、行注释/移动/复制与单次撤销 | 通过 |
| 空白草稿格式/语言只存布局，不制造checkpoint | 通过 |
| 格式切回清除格式dirty；保存中再改格式不被旧保存抹掉 | 通过 |
| 仅BOM修改重启仍dirty，实际保存产生`EF BB BF` | 通过 |
| 最近文件/偏好重读及SQLite shutdown确认 | 通过 |

原有完整场景继续通过：[usability-full-after.json](evidence/usability-full-after.json)。各场景报告包含公共字段，未执行的其他场景布尔字段为false，不代表当前场景失败；以`scenario`及`passed`区分。

视觉证据：[补齐前](evidence/usability-before.png)、[补齐后默认空白窗口](evidence/usability-after.png)。后半段macOS锁屏使外部键鼠注入和窗口截图不可靠；曾生成的黑色面板截图不作为证据，独立验证者最初把静态推断当作交互观察的表述已撤回。本轮不宣称完成解锁后的鼠标逐项验收、系统Option键行为或真实IME验证。

最终独立复验也通过：[日常功能](evidence/usability-verifier.json)、[完整场景](evidence/usability-full-verifier.json)、[同目录重启](evidence/usability-restart-verifier.json)。重启恢复4标签，只hydrate活动空白草稿；最终二进制及锁文件标识单独保存在[本轮标识](evidence/usability-build-identity.txt)，不覆盖前一轮性能基线。

审查边界：UI实现及格式/恢复排序经`design-review`复审通过。独立偏好存储的`adversarial-check`两次因模型服务网络/流停顿失败，未产生结论，不能记为独立对抗审查通过；主协调者检查了有界队列、flush/shutdown及原子发布路径，现有存储故障测试和原生重启验证通过。

按键派发回归加入后，最终`cargo fmt --all -- --check && cargo test --locked --workspace`仍通过（92测试）：[最终检查输出](evidence/usability-final-checks.txt)。本轮临时会话、夹具、Swift脚本及无效截图已清理；保留原始报告、有效前后截图与release二进制。

## 公开仓库与五目标CI

公开仓库已建立：[stabey/makeaedittool](https://github.com/stabey/makeaedittool)，初始审计提交`1af1247`。发布前移除了测试日志中的本地会话标识，排除了无关桌面截图；保留实际测量与资源归属声明，补齐Apache-2.0根许可证。

CI设计与实现经`CiDesignReview`评审。`actionlint 1.7.12`发现并修复了job级`runner.temp`上下文错误；随后删除不必要的输出目录环境设置。首个运行`42397198407`被新推送自动取消，不将取消记作平台功能失败。

[首轮完整托管运行42397473795](https://github.com/stabey/makeaedittool/actions/runs/42397473795)在五个原生目标上均完成workspace测试、release构建及无窗口CLI验证：macOS ARM64/Intel各92测试，Windows x64/ARM64及Linux x64各85测试。差异来自已有macOS保存测试条件，不是通过临时跳过失败测试获得。CI不运行原生GUI smoke。

该轮最终失败于打包，而非编译：Cargo源包的部分许可文件时间戳早于1980年，Python ZIP默认严格时间戳检查拒绝写入，故**没有上传任何产物**。失败证据：[ci-first-failure.json](evidence/ci-first-failure.json)、[原始日志](evidence/ci-failures.txt)。修复只将ZIP不可表示的时间戳钳制到格式边界，不改变文件字节或权限；新增真实ZIP回归验证1970年许可文件的字节与模式保留，本地7个打包测试、actionlint与Rust格式检查通过。
