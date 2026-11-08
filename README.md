# Native text editor

Rust + GPUI Kit 文本编辑器。产品与验收基线见 [交接文档](Rust_GPUI_Editor_Agent_Handoff_v1.0.md)，当前状态见 [IMPLEMENTATION_STATUS](IMPLEMENTATION_STATUS.md)。默认应用与NSH候选实验隔离；本地原生GUI验收目前仅覆盖macOS Apple Silicon，CI构建不等于完整平台支持。

产品定位是接近Notepad++的纯文本编辑器：本体优先保证编辑、保存恢复、大文件、多文件、标签页组及单窗口分屏。项目模型和LSP不作为本体目标；未来如需这些能力，通过插件提供。当前已实现标签页组和不同文档的单窗口分屏；同一文档的独立多视图尚未实现。

## 构建环境

- Rust 1.99.1（`rust-toolchain.toml`）。
- macOS 完整 Xcode、Command Line Tools、Metal Toolchain。`xcode-select -p` 与 `xcrun -f metal` 应可用。
- 初次 Cargo 构建需要已缓存依赖或联网获取；锁文件固定解析结果。应用基础运行不需要 Node/npm。

```sh
cargo build --locked --release -p editor
cargo run --locked --release -p editor -- --data-dir .smoke-data/session
cargo test --locked --workspace
```

首次依赖获取不是应用启动性能；构建成功也不代表 IME、恢复或保存故障验收通过。

## GitHub与多平台构建

源码：[stabey/makeanotepad](https://github.com/stabey/makeanotepad) · [Native CI](https://github.com/stabey/makeanotepad/actions/workflows/ci.yml)

推送`main`、Pull Request及手动触发均执行锁定依赖的测试和release构建。五个目标分别使用原生托管runner：macOS ARM64、macOS Intel、Windows x64、Windows ARM64、Linux x64；不会把ARM模拟x64的结果记作原生x64验收。

在成功运行的 **Artifacts** 中下载对应目标的ZIP，保留14天。产物含`editor`/`editor.exe`、服务CLI、构建身份、SHA-256、目标相关许可证清单及许可文件。它们是开发构建，不是安装包或正式签名/公证版本；系统运行库仍由目标环境提供。

**Windows/Linux当前不能保存用户文件。** 安全保存实现仅支持macOS；其他平台产物显著携带`EXPERIMENTAL-CANNOT-SAVE.txt`，只用于构建与兼容性验证，不应作为日常编辑器使用。草稿恢复不能替代保存文件。

CI运行无需窗口的`--help`、`--version`、100文档会话验证，并在macOS额外运行保存/故障服务验证。它不执行GUI smoke，不证明真实IME、鼠标交互、GPU兼容或性能指标。具体状态以运行日志及ZIP中的`BUILD_IDENTITY.txt`、`HEADLESS_SMOKE.json`为准。

## 工程结构

- `apps/editor`：Kit 原生窗口、普通编辑适配层、标签与分页查看。
- `crates/editor-services`：不依赖 GPUI 的文件、分页、搜索、保存和恢复服务。
- `spikes/nsh-eval`：独立 headless / Kit Fast + TSX 候选实验；不属于默认 workspace。
- `docs`：架构决定、测试与性能记录。

## 日常使用

默认显示 **Open Editors / Recent Files** 侧栏；最近文件最多30项，列表过滤不打开文件、不预加载未激活标签。侧栏不是项目目录树。

| macOS 快捷键 | 操作 |
|---|---|
| `Cmd+Shift+P` / `F1` | 命令面板 |
| `Cmd+P` | 已打开/最近文件快速切换；输入 `>` 切换命令模式 |
| `Cmd+B` | 显示/隐藏侧栏 |
| `Cmd+Shift+D` / `Cmd+Shift+K` | 复制行 / 删除行 |
| `Alt+↑` / `Alt+↓` | 上移 / 下移当前行或选中行 |
| `Cmd+/` | 支持语言的行注释切换 |
| `Tab` / `Shift+Tab` | 原生缩进 / 取消缩进 |
| `Alt+Z` | 自动折行 |

自定义复制/删除/移动/注释命令只处理**活动选区**，会收敛其他光标；原生缩进保留Kit自身的多光标行为。单次行操作跨度/输出限制4 MiB，超限拒绝而非部分修改。JSON、纯文本、CSS、HTML、XML、Markdown不提供伪行注释。

底部状态栏可选择语言、LF/CRLF、UTF-8/BOM、空格/Tab及2/4/8宽度，并切换折行、空白字符显示。语言和显示选项不修改文件；编码/换行变化会标记已保存文件为未保存，并进入恢复保护。只读/分页文档禁用不适用的写入选项。

最近文件与显示偏好保存在数据目录的 `ui_shell.json`；会话文本仍由独立SQLite恢复库负责。偏好后台合并写入，可见窗格的编辑器即时应用，不可见标签在变为可见时惰性应用。损坏偏好文件在首次替换前保留独立备份，不因启动而直接覆写。

### 标签页组

使用标签栏的 **+ Group**、原生 **Groups** 菜单或命令面板创建组。组支持名称、颜色、折叠和排序；拖动标签可调整组内顺序或移到其他组，拖动组标题可重排组，`Escape`取消拖动。

| macOS 快捷键 | 操作 |
|---|---|
| `Cmd+Alt+G` / `Cmd+Alt+Shift+G` | 当前标签新建组 / 创建空组 |
| `Cmd+Alt+R` / `Cmd+Alt+C` | 重命名当前组 / 切换组颜色 |
| `Cmd+Alt+M` | 折叠或展开当前组 |
| `Cmd+Alt+[` / `Cmd+Alt+]` | 在组内前移 / 后移当前标签 |
| `Cmd+Alt+Shift+[` / `Cmd+Alt+Shift+]` | 将当前标签移到前一组 / 后一组 |
| `Cmd+Alt+U` | 将当前标签移回未分组 |

折叠只隐藏标签，活动文档仍保持打开并在组标题中提示；侧栏和快速切换仍能访问组内成员。**Remove Group (Keep Tabs Open)**只解除分组；**Close All Tabs in Group…**是独立操作，汇总确认后才关闭，取消或保存失败保留整组标签。关闭确认期间暂时冻结编辑与结构操作，避免保存或关闭作用于已经变化的内容。

组名最多128个UTF-8字节，最多128个自定义组；当前窗口最多1024个文档视图。移动和分组不复制文本，不重置撤销历史。组、顺序、选择与视图位置由同一个SQLite恢复worker持久化，不放入`ui_shell.json`。

**File → Recovery Draft Catalog…**可分页查看持久草稿和未接纳的恢复记录。达到1024个视图上限后，可先保留草稿并关闭一个视图，再打开其他记录；**Export Copy…**不占用新视图、不更改原草稿的保存关联。损坏记录明确标为不可访问，不冒称已经修复。

### 单窗口分屏

使用 **Panes** 菜单、工具栏或窗格菜单向左/右/上/下拆分；最多8个窗格，每个窗格有自己的标签栏和当前文档。普通拆分创建空窗格，**Split & Move Tab…**把当前标签移入新窗格，二者都不会复制文档。打开已经存在的文档会定位到它所在的窗格。

| macOS 快捷键 | 操作 |
|---|---|
| `Cmd+\` / `Cmd+Shift+\` | 向右 / 向下拆分 |
| `Cmd+Alt+\` / `Cmd+Alt+Shift+\` | 向左 / 向上拆分 |
| `F6` / `Shift+F6` | 聚焦下一个 / 上一个窗格 |
| `Cmd+Alt+Shift+N` / `Cmd+Alt+Shift+P` | 将当前标签移到下一个 / 上一个窗格 |
| `Cmd+Alt+=` | 均衡窗格大小 |
| `Cmd+Alt+Z` | 仅显示焦点窗格 / 恢复完整布局 |
| `Cmd+Alt+X` | 关闭当前窗格，并合并标签到相邻窗格 |

拖动分隔线调整尺寸，`Escape`取消此次调整；拖动标签到另一窗格中央可移动，到边缘可拆分并移动，到组标题可重新分组。预览不修改正文；无效目标、容量不足或取消不会留下半完成的移动。

**Close Pane (Merge Tabs into Adjacent Pane)**不会关闭文档；**Close All Tabs in Active Pane…**才是需要草稿保护确认的关闭操作。点击正文后，输入、保存、搜索和行操作作用于该窗格的当前文档。窗格树、比例、标签位置和选择会恢复，文本实体与撤销历史不会因移动而复制或重置。

交互拆分保证每个窗格至少240×160逻辑像素；窗口太小时显示 **Compact** 提示并暂时只显示焦点窗格，放大后恢复原布局，不删除隐藏窗格。所有窗格共享原有内容、页缓存和索引预算；恢复先加载焦点文档，再逐个加载其他可见文档，预算不足的标签保留并显示原因。

## 数据安全

普通文档的修改与用户文件保存分开：草稿恢复成功不能清除 dirty。恢复文档不自动覆盖磁盘来源。只读分页文件不会全文复制进恢复库。

使用 `--data-dir` 可将验证数据与个人会话隔离。卸载二进制不应自动删除恢复数据；手动删除数据目录前先导出需要的未保存草稿。生成的大文件和临时测试会话不提交到版本控制。

旧会话仍可原子迁移到SQLite schema v2；本轮保持数据库schema不变，将布局格式扩展到format v2以保存分屏。format v1在内存中升级，正常布局写入时才产生新快照。升级前请关闭旧实例并备份整个数据目录；旧版不能读取新布局，保留旧快照不等于支持直接降级。未知未来布局格式拒绝写入；已知格式的损坏布局保留原始记录，再尝试最近有效布局和元数据恢复。

不传 `--data-dir` 时，macOS恢复目录为 `~/Library/Application Support/com.makeaedittool.editor`，数据库文件为 `recovery.sqlite3`。不要将它放到网络共享目录或同步盘。普通编辑器合计内容准入默认256 MiB、最多128个已hydrate编辑器；这些不是RSS硬上限，撤销历史和布局缓存另占内存。单文件超过64 MiB默认只读分页，不冒充任意大文件可编辑。

原生自动化验证使用隔离目录，报告在正常退出与恢复shutdown确认后写出：

```sh
target/release/editor --data-dir .smoke-data/native-full --smoke=full --smoke-report .smoke-data/native-full.json
```

## 候选运行时

```sh
cargo run --locked --release --manifest-path spikes/nsh-eval/Cargo.toml --bin nsh-eval
cargo run --locked --release --manifest-path spikes/nsh-eval/Cargo.toml --features panel --bin nsh-panel-eval
```

结果与上游兼容性问题见 [NSH_EVALUATION](docs/NSH_EVALUATION.md)。默认编辑器不因候选失败改用未验证依赖。

## 证据

- [架构决策](docs/ARCHITECTURE_DECISIONS.md)
- [测试记录](docs/TEST_REPORT.md)
- [性能记录](docs/PERFORMANCE_REPORT.md)
- [已知限制](docs/KNOWN_LIMITATIONS.md)
- [主应用依赖许可证清单](docs/DEPENDENCY_LICENSES.csv)是本地macOS解析快照；[隔离NSH候选清单](docs/NSH_DEPENDENCY_LICENSES.csv)仅对应实验。它们包含开发依赖，不等于实际链接清单。每个CI产物另生成目标过滤后的`DEPENDENCY_LICENSES.csv`及`licenses/manifest.json`，并附可获取的上游许可与资源声明。
- 内置资源另有许可证：[Lucide/Feather图标](docs/licenses/gpui-kit-assets-0.9.0-LICENSE-LUCIDE)、[two-face 0.4.5语法与主题声明](docs/licenses/two-face-0.4.5-acknowledgements.md)。后者保留固定上游版本的完整声明，不能只用crate级MIT/Apache标签替代资源许可。

所有尚未实际运行的能力均按未验证处理，不将交接文档目标或上游组件能力当作本项目成绩。
