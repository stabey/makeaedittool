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

[修复后运行42399981386](https://github.com/stabey/makeaedittool/actions/runs/42399981386)对应提交`1536938d54414f40a360eb245baf1af0ddfcb739`，全部6个job成功：7个打包测试及格式检查通过，五平台workspace测试数量同上，release、CLI和上传均成功。五份ZIP已实际下载，核验二进制架构、SHA-256、提交标识、目标专属许可证清单及平台警告。每个CLI会话场景为100标签、只hydrate 1个文档；不是跨平台GUI或性能基准。

独立验证器运行了**下载的**Mac ARM产物，而非本地旧二进制：`--smoke=usability`及`--smoke=full`均报告`passed=true`、`in_app_checks_passed=true`、`shutdown_durable_ack_observed=true`。数据目录隔离，未使用默认用户数据；完整场景覆盖事务/撤销、过期版本拒绝、Unicode搜索、CRLF/BOM、关闭重开、丢弃恢复及分页搜索72处匹配。程序化原生窗口smoke不等于硬件输入、拖拽或真实IME验收。

最终证据：[ci-verification.json](evidence/ci-verification.json)、[ci-native-usability.json](evidence/ci-native-usability.json)、[ci-native-full.json](evidence/ci-native-full.json)、`ci-final-*.txt`。Windows/Linux保存仍为Unsupported；各自ZIP带`EXPERIMENTAL-CANNOT-SAVE.txt`。其他平台原生GUI、签名/公证、安装包与正式Release发布不在此次已验证范围。

## 单窗格标签组与版本2会话

最终本地执行`cargo fmt --all && cargo test --locked --workspace && cargo build --locked --release --workspace`通过：122测试。最终二进制及每条测量命令见[group-accepted-summary.json](evidence/group-accepted-summary.json)；editor SHA-256为`0d439c7f069065828326b87cbff836143228ef91daf0ece720ec4f32b524c896`。

实际原生窗口运行通过：[标签组](evidence/group-accepted-groups.json)、[1000草稿整组操作](evidence/group-accepted-bulk.json)、[完整功能](evidence/group-accepted-full.json)、[日常功能](evidence/group-accepted-usability.json)、[启动](evidence/group-accepted-startup.json)。均为独立数据目录，`passed`与退出持久化ACK为true。各分支未执行的公共布尔字段为false，不冒充覆盖。

- 标签组：批量打开的首路径失败、后续有效路径与去重；分组元数据不改正文/撤销；关闭冻结、取消恢复、原生待提交编辑保护、关闭重开及重启布局。
- 使用GPUI公开API对已绘制窗口派发真实鼠标事件，执行标签跨组、组重排、标签锚点拖放及Esc中止；通过快捷键输入组名、重命名、折叠、打开恢复目录并检查弹层焦点恢复。小会话及1000草稿会话均通过三项native UI标志。
- 1000草稿只hydrate活动正文，真实窗口动作打开汇总对话框；一次取消保留身份、布局、checkpoint与可编辑状态，一次保留关闭等待ACK后留下新空白。主协调者另以只读SQLite查询确认：`state=1`为1000条，`state=0`为1条。重启逐条检查关闭状态、恢复头和原位置。
- 100/1024标签各三次迁移与重启通过，始终只hydrate一个69632字节正文；[满容量CLI打开](evidence/group-accepted-overflow.json)仍正确hydrate原活动文档。

失败与修复保留，不只记录通过：

1. 中间Groups smoke因Undo派发前缺少真实帧而超时；补帧后通过，未放宽文本/撤销断言。[原失败](evidence/group-final-smoke-groups.json)与[修复后中间版本](evidence/group-final-smoke-groups-ce4871.json)分别保留。
2. [1000草稿首次失败](evidence/group-native-bulk.json)暴露真实产品问题：Kit的Editor默认readonly=false，重绘覆盖生命周期冻结。渲染现在显式传入当前readonly，回归检查在绘制后尝试原生输入并要求正文、版本和撤销不变；最终正常及批量场景均通过。
3. 对抗审查发现未来布局额外字段可能被serde忽略，以及旧损坏快照可能在轮换时被清理。修复在写连接前检查两个布局头的未知字段，并在轮换前保存已知格式损坏原始字节；新增回归覆盖不改DB/WAL、事务回滚与两次写入后保留。针对性复审无剩余阻断；复审者未运行测试，测试由主协调者执行。

外部AppleScript发送成功曾被验证器误报为操作成功，已撤回并纠正[group-final-ui-interactions.txt](evidence/group-final-ui-interactions.txt)。当时数据库仍为1000条Open，不能证明关闭或取消。上述最终验证改用窗口命中测试和实际状态断言；仍不宣称物理HID、真实IME或像素级呈现验收。桌面截屏全黑，ScreenCaptureKit窗口捕获失败`-3811`。

首轮标签组CI [42494337544](https://github.com/stabey/makeanotepad/actions/runs/42494337544)：macOS Intel、Windows x64/ARM64和Linux完成测试、release、CLI及打包；macOS ARM在测试辅助函数中直接unwrap清单请求，遇到服务有界队列的正常Busy而失败。修复仅让该辅助函数沿用已有有界`admit`协议，不扩大生产队列、不忽略操作错误、不重跑碰运气；本地62个服务集成测试通过。原日志见[group-ci-first-failure.txt](evidence/group-ci-first-failure.txt)。

修复后[42496139274](https://github.com/stabey/makeanotepad/actions/runs/42496139274)对应`734f8d3998e423e85145b46e4c1f83a273da2fef`：格式检查及五个原生平台job全部成功，均完成workspace测试、release构建、无窗口CLI、二进制架构/校验值/许可检查和unsigned开发ZIP上传。结构化结果见[group-ci-final.json](evidence/group-ci-final.json)。本轮新CI产物未下载运行；原生交互与性能证据来自前述本地最终release，不能混称CI产物验收。任务生成的临时数据库、夹具与无效截图已清理，默认用户数据未用于验证。

## 不同文档分屏交付

- 最终执行 `cargo fmt --all && cargo test --locked --workspace && cargo build --locked --release --workspace`：138个测试通过，release构建成功。
- 最终二进制身份见[evidence/split-final-identity.json](evidence/split-final-identity.json)。其中base HEAD仅表示开发基线，不冒充未提交源码身份。
- 最终原生窗口场景 `splits`、`usability`、`groups`、`full` 均exit 0、passed及durable shutdown ACK为true，见`evidence/split-final-accepted-*.json`及对应`-os.txt`。
- 分屏覆盖2/4/8窗格、拒绝第9窗格、四方向拆分、真实窗口内拖放及Esc取消、240×160最小叶区域、缩小/展开与焦点模式、均分、输入/撤销/保存隔离、分页视图焦点、搜索焦点及resize取消、关闭合并。保护文档的“关闭本窗格标签”实际渲染确认框，验证全会话readonly屏障，Cancel恢复全部身份/正文/修订/undo/readonly；同窗格Keep仅移除目标标签，保留其他窗格及空窗格，并验证重启布局。
- 原生验证先暴露并修复三项问题：等待帧回调不能保证尺寸变化已落地；右上提示遮住标签拖放起点；边框内测量238px与外框240px混淆。保留`split-final-native-first/second/third.json`失败报告，未放宽240px约束；第四次及最终新增生命周期覆盖通过。
- 独立回归发现旧usability夹具把Dehydrated文档留作活动标签跨越await，新可见窗格加载机制因此合法加载它。夹具改为先恢复另一个活动标签，再替换非可见stub，并显式断言前置条件；原有过滤不能加载正文的断言保留，最终复测通过。失败报告`split-verifier-usability.json`保留。
- 100/1024标签×1/2/4/8窗格：24次startup、8次输入场景全部通过；每次实际测量可见窗格数量，不以逻辑树数量替代。额外100标签4窗格、最大1个hydrate预算场景通过，焦点优先且只有69,632 bytes正文。性能口径见性能报告。
- 独立设计及最终UI复核无阻塞。应用窗口截图仍失败，诊断见`evidence/split-verifier-capture-failure.txt`；不宣称像素、物理HID/IME、Windows ARM GUI或性能通过。本轮未重跑64 GiB扫描。

分屏提交`f1c1507170c7f4cb2499bee8ddec51cf05d06efa`的[五平台CI 42510374861](https://github.com/stabey/makeanotepad/actions/runs/42510374861)全部成功：macOS ARM/Intel、Windows ARM/x64、Linux x64，以及格式检查。五个平台均执行workspace测试、release构建、无窗口CLI、架构/校验值检查并上传unsigned开发ZIP；结构化结果见[evidence/split-ci-final.json](evidence/split-ci-final.json)。本轮未下载运行CI产物，不将其视为跨平台GUI验收。任务创建的分屏夹具数据库和失败smoke残留文件已清理，原始JSON/资源日志保留，默认用户数据未使用。


## 同文档多视图：隔离可行性实验

实验独立于生产工作区，使用固定版本的真实GPUI Kit和已有公开服务/适配器；没有修改生产代码、Cargo注册表或恢复schema。嵌套锁文件从根锁选择起步，并按macOS ARM目标核验Kit/base/component 0.9.0、gpui-pre 0.3.11、Ropey 2.0.0-beta.1的版本及校验值。全目标offline metadata曾因未缓存的`rsqlite-vfs`失败，限定实际macOS目标后成功；没有升级依赖规避问题。

### 普通编辑器诊断

release二进制实际创建原生窗口，使用真实`EditorAdapter`、`Editor`和输入回调。覆盖两份独立状态的反例、Entity别名、不可变Rope快照、`set_value`清空历史、`n → ni → 你`提交与Undo/Redo、单独unmark、空标记取消、组合中焦点切换。每个输入前聚焦绘制，每步独立派发/刷新；unmark和取消之后只绘制，不追加编辑掩盖状态。

首次执行完成66个阶段，退出码2；全部诊断边界断言通过，6个产品安全断言失败，不能标记为全通过：

| 原生场景 | 实际观察 |
|---|---|
| IME Undo/Redo | 每次产生3个编辑事件，适配层committed快照及`build_recovery_commit()`载荷依次出现`ani`/`an`；最后事件恢复正确最终正文 |
| 仅unmark | mark已清除，原生正文`an`，committed仍为`a`，无编辑事件/恢复载荷，身份屏障持续为true |
| 空标记取消 | 两边正文均为`a`，无新增revision/dirty/恢复载荷，但Rope实例不同，身份屏障持续为true |

后两项分别在立即观察和纯绘制后观察失败；加上Undo/Redo共6项。窗口及11个原生编辑器弱引用均释放，零剩余窗口、无清理错误。见`evidence/multiview-shared-text-first.json`及对应OS日志。这里构造的是内存恢复载荷，未运行RecoveryHandle、SQLite、工作区Save/Close/Quit；不能宣称中间快照已经写盘，也未证明真实输入法会走每种合成回调序列。

正向目标编译/运行后，单独运行带`--features private-probe --bin private_state_probe`的release `cargo check`，退出101且仅有两个预期`E0616`：`text`及`undo_manager`不可公开访问。目标以非默认feature隔离，不破坏正常构建。命令及诊断见`evidence/multiview-private-probe.{json,txt}`。

### 共享源分页原生实验

最终release的1/2/4/8视口各3次串行执行全部exit 0、passed true。每次均验证真实布局最小尺寸、虚拟列表裁剪、原生窗口内滚轮独立滚动、搜索焦点/高亮归属、单源缓存去重、完整索引、关闭至1和最后关闭、取消隔离、64位偏移及所有视口的源变化失效。8视口为2×4网格，实际叶区域360×416，列表358×388逻辑像素。矩阵及逐次路径见`evidence/multiview-final-matrix.json`。

首次8视口失败来自断言写死`@0x1000`前缀，错误拒绝正确的`@0x100100000`；改为与期望完整64位anchor一致后通过。保留`multiview-paged-8v-first.json`，未放宽偏移、marker或continuation约束。

复核要求的修正均在隔离实验内：检查真实handle的最新source generation；通过持有真实后台读取的异步完成结果，分别跨越请求替换和源失效，验证迟到结果不能覆盖当前视口，不再用同步手工调用冒充异步完成；区分取消与过期结果。1视口加载计时与其他N同口径，搜索槽位记录原始采样值而非强制至少1。前台I/O字段改为源码检查说明，不使用硬编码0冒充运行监测。

临时夹具使用固定版本`tempfile`的原子目录所有权；正常清理等待后台操作、搜索及共享源释放，不以固定睡眠假定完成。panic或任务排空失败保留并报告自有残留，不在工作任务仍活动时递归删除。报告写入失败仍输出并flush诊断；拒绝含缺失路径后`..`的歧义报告路径。已执行非法视口数、移除的scratch选项、不可写报告目标、歧义报告路径四个负面场景，均非零退出并输出JSON/失败诊断，Main自建的既存目录sentinel始终保留；见`evidence/multiview-negative-cases.json`。

调查中曾怀疑旧scratch参数会删除调用者目录；只在Main自建目录运行后，sentinel实际保留，证明旧实现创建的是独立子目录，**未复现调用者目录删除**。后续去掉该非必要选项并改用TempDir是收紧实验所有权边界，不宣称修复已发生的数据丢失。

本阶段不修改主应用/服务，不重新宣称五平台GUI或生产多视图通过；隔离spike不在现有五平台CI工作区成员中。真实物理IME、HID、像素呈现、长期资源分布、共享可编辑文档及布局迁移仍未验收。

最终独立运行验证与Main结果一致：`multiview-verifier-final-shared-text.json`完成66阶段、诊断边界通过、保留相同6个产品安全失败（exit 2）；`multiview-verifier-final-paged-8v.json`全部门槛通过（exit 0），真实异步旧请求及旧source generation完成各被拒绝，最后源/缓存/索引归零。对应`-os.txt`保留实际命令输出；最终二进制SHA-256与矩阵一致，见`multiview-final-identity.json`。

实验源码及原始证据提交为`537db63`；未改变生产工作区，因此本次提交显式跳过原五平台CI，不能把旧CI结果算作这两个新spike的跨平台构建证明。两个spike的实际证据是上述本地macOS release编译、原生运行和独立复核。任务生成的夹具及sentinel已清理；未使用默认用户数据目录。


## 单编辑器IME结算与原子历史修复

本轮使用受维护的`gpui-base 0.9.0`路径补丁，版本不升级、应用层`EditorAdapter`生产逻辑不变。行为修改及私有测试仅在两个原生源文件；来源、修改边界和回退规则见ADR011与vendor NOTICE。设计、实现契约及针对性历史丢失审查均无剩余阻塞。

- 工作区158个测试通过，包含20个公开原生行为用例；移除编译器指出的冗余导入、完成格式化后，20个原生用例再次通过。记录见[工作区测试](evidence/ime-workspace-tests.txt)与[最终定向复验](evidence/ime-native-tests-final.txt)。不是把定向重复执行再累加成新的测试数量。
- 显式vendor私有测试11个通过、1544个过滤，见[历史pin测试](evidence/ime-private-history-tests.txt)。命令必须启用`--features test-support`，过滤路径是`input::undo_manager::tests::ime_pin_`；根`--workspace`不会运行这些用例。CI的macOS ARM步骤已使用该命令。
- release主应用及两个活动实验均实际编译。根与两个实验的解析结果均只有一个本地`gpui-base 0.9.0`，原有包名/版本集合未升级；现有打包函数实际找到原LICENSE及`NOTICE-IME-PATCH.json`。二进制、锁文件及原生源文件标识见[本轮身份](evidence/ime-final-identity.json)。
- [原生诊断](evidence/ime-native-diagnostic-final.json)完成66阶段、exit 0：56个诊断边界和12个产品断言均通过，包含未改动的6个原失败断言。两个未修改版本的旧`source_prediction`不再成立，不是产品失败；不把它们表述为仍满足。
- 独立验证器运行[真实工作区IME场景](evidence/ime-workspace-final.json)，五个IME验收标志、`passed`及durable shutdown ACK全部为true。覆盖原生逐事件最终快照、preedit期间Save/Close Busy及搜索隔离、零revision取消、单独unmark、first-baseline和已有checkpoint的实际SQLite提交/hydrate、BOM/CRLF真实文件保存、关闭确认框readonly/Cancel/Keep/重开/Discard，以及worker关闭后的新RecoveryHandle重开验证。[OS输出](evidence/ime-workspace-final-os.txt)保留进程指标。
- [完整](evidence/ime-regression-full.json)、[日常功能](evidence/ime-regression-usability.json)、[标签组](evidence/ime-regression-groups.json)、[分屏](evidence/ime-regression-splits.json)、[启动](evidence/ime-regression-startup.json)顺序运行，均exit 0、passed且观察到正常退出durable ACK。
- 另起真实进程使用同一个IME数据目录，恢复3个标签，只hydrate活动的8 B草稿，正常退出通过，见[同会话重启](evidence/ime-same-session-restart.json)及[OS输出](evidence/ime-same-session-restart-os.txt)。IME场景已删除其拥有的磁盘夹具；重启没有靠重新读取这些夹具填充正文。
- 另一受影响调用方的[8视口分页实验](evidence/ime-paged-eight-view.json)也以新补丁实际运行，exit 0、全部门槛通过，关闭至零及scratch清理通过；[构建及OS输出](evidence/ime-paged-eight-view-build-os.txt)保留命令结果。本次不是重跑整个1/2/4/8性能矩阵。

本机为macOS 27.2 / Darwin 27.2.0、ARM64、32 GiB；没有从Darwin版本推算产品版本。1/16/近64 MiB各192次提交的细分延迟与纯打开内存对照见性能报告。真实系统IME候选、重转换、物理HID、屏幕延迟、Windows/Linux GUI与性能仍未验收；Windows/Linux安全保存仍不支持。多平台CI结果另按本次提交记录，不从本地运行外推。

生产修复提交`dd70272319249d4f5bb2ef1ebd1f592c9196311f`的[CI 42590963834](https://github.com/stabey/makeanotepad/actions/runs/42590963834)全部6个job成功：格式/打包检查，以及macOS ARM/Intel、Windows ARM/x64、Linux x64的测试、release、CLI、架构/校验值检查和unsigned开发ZIP上传。[运行结果](evidence/ime-ci-final.json)与[5份产物元数据](evidence/ime-ci-artifacts.json)绑定同一提交；[macOS ARM日志摘录](evidence/ime-ci-private-tests.txt)确认私有筛选实际执行11个测试，而不是零测试成功。

已下载**本轮CI的Mac ARM产物**，两份可执行文件的SHA256SUMS验证通过；其`gpui-base`原LICENSE和修改NOTICE均存在，NOTICE内容与维护源一致。该产物在本机执行`--smoke=ime`也exit 0，五个IME标志、passed及durable ACK全部为true，见[产物原生报告](evidence/ime-ci-macos-native.json)、[校验及OS输出](evidence/ime-ci-macos-native-os.txt)、[构建身份](evidence/ime-ci-macos-build-identity.txt)。构建身份中的`gui_smoke_executed=false`描述托管CI；下载后本地执行单独记证。未下载运行其余平台GUI，未宣称签名、公证或正式Release完成。

本轮原始报告已归档；任务拥有的运行夹具、SQLite会话及下载验证临时目录已清理，未使用或删除默认用户数据目录。

## 普通加载Rope接管与顺序行布局

ADR012实现与复审无剩余阻塞；不改变IME事件/历史协议、应用层身份屏障、可编辑准入、恢复/保存或布局schema。维护源为原IME两文件加`text_wrapper.rs`，新增公开接口仅为EditorMode的新建构造器。

- [集成测试日志](evidence/load-tests.txt)：`cargo test --locked --workspace`为160通过；两个新增原生用例覆盖准确字节/Rope身份、零加载事件/历史、首次编辑Undo/Redo、UTF-8/CRLF/EOF光标恢复及首次draw后的光标可见性。
- 显式vendor行布局模块19通过，包含四个新增边界用例；IME历史pin模块11通过。行布局覆盖LF/尾空行/CR/Unicode/跨块、换行拆分/合并/全删、行摘要及坐标映射、混合宽度复用和续行绝对字节位置。CI的macOS ARM新增同一行布局筛选命令，不依赖被排除vendor自动进入工作区。
- [构建日志](evidence/load-builds.txt)：release工作区及两个活动实验均成功。原包比对仅有三个维护源和独立测试锁文件差异，额外NOTICE记录用途和来源；修改后源SHA与NOTICE相符。
- [真实工作区报告](evidence/load-native-regressions.json)：startup/full/ime/usability/groups/splits六场景均passed并取得durable shutdown ACK。IME五项验收标志均为true，真实编辑/撤销、BOM/CRLF保存、关闭取消/保留/丢弃及恢复路径没有回归。
- [相同夹具交替对照](evidence/load-startup-paired.json)：短行旧/新release各3次，长行形状对照各1次；每次都实际hydrate一份67,108,860 B普通正文，0个分页标签，显式draw后正常退出。性能与缓存限制见性能报告，不用更早的就绪时间替代总工作量。
- [共享文本诊断](evidence/load-shared-text-diagnostic.json)：66阶段、exit 0，56个诊断边界及12个产品断言均满足；3个历史源码预测仅1个仍成立，不将旧预测计为产品失败或全部满足。
- [分页8视口实验](evidence/load-paged-eight-view.json)实际运行通过；不是重跑完整1/2/4/8性能矩阵。
- [新版实际绘制采样](evidence/load-sample-after-draw.txt)绑定PID 9697的`--smoke=startup`进程，[同进程报告](evidence/load-sampled-startup.txt)passed、durable ACK及67,108,860 B正文均确认。采样开销下的时间不并入未插桩性能均值。另一个无强制draw后台进程的148.6 MiB被排除出稳定内存对照。

只验证本机macOS ARM64。没有物理系统IME、像素首帧、长期多文档稳定堆、Windows/Linux GUI或性能的新结论；Windows/Linux保存仍不支持。

使用上述IME场景的同一数据目录另起真实进程，`--smoke=startup`恢复3个标签、仅hydrate活动8 B草稿并取得durable ACK，见[同会话新进程记录](evidence/load-same-session-restart.txt)。这额外覆盖服务层恢复Rope经新构造器进入原生编辑器的路径，不仅在同一进程打开RecoveryHandle。

生产优化提交`4f3087fb5711d29b5d703237eda00ff509dad96c`的[CI 42845274569](https://github.com/stabey/makeanotepad/actions/runs/42845274569)全部6个job成功：格式/打包检查及macOS ARM/Intel、Windows ARM/x64、Linux x64五个原生目标。各目标测试、release、CLI、校验和未签名ZIP上传均成功；[最终运行](evidence/load-ci-final.json)、[5份产物元数据](evidence/load-ci-artifacts.json)及[Mac ARM私有测试日志](evidence/load-ci-private-tests.txt)绑定该提交，确认行布局19个和历史11个实际执行，不是零匹配。

下载的本轮Mac ARM产物两个二进制SHA校验通过，携带原Apache LICENSE及与维护源一致的三文件NOTICE。[构建身份](evidence/load-ci-macos-build-identity.txt)中的`gui_smoke_executed=false`描述托管CI；下载后本机另行运行近64 MiB `--smoke=startup`及完整`--smoke=ime`，均exit 0、passed并取得durable ACK，见[产物运行报告](evidence/load-ci-macos-native.json)。其他平台没有新增GUI或性能验收。

运行证据归档后，任务独占的`/tmp/runtime_verify_profile`及Mac ARM产物下载临时目录已清理；仅保留源码、release构建与证据，没有使用或删除默认用户数据目录。
