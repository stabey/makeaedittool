# 性能记录

## 测量环境（实际探测）

- 原生 macOS 27.2，build 26B123；Apple M4，10 CPU，32 GiB RAM。
- 工作目录所在卷：**外置 USB SSD，APFS**，非内置 NVMe。实验数据写入此卷时必须标明 USB；不能据此声称通过交接文档的 NVMe 冷读目标。
- Rust 1.99.1；release 构建。首次 Cargo 下载/编译不计入应用启动。
- Windows 11 为 Parallels ARM64 guest，不纳入 Windows x64 性能结果。

## 指标定义

1. 存储首范围延迟与原生 GUI 首屏绘制分别测量；读取 64 KiB 完成不等于窗口可输入。
2. 进程冷启动不等于文件系统冷缓存。没有清除 OS 缓存的测量标为 warm/unspecified，不冒称冷盘。
3. 普通输入提交延迟不等于端到端屏幕显示延迟；IME 与候选窗口单列。
4. 搜索吞吐必须同时报告实际扫描字节、是否达到结果上限、原始顺序读基线、算法、块长和取消条件。
5. 资源报告需区分全局缓存预算、实际占用、进程 peak RSS、关闭后 RSS。分配器保留内存不自动等于存活对象泄漏。
6. 100 标签启动应记录活动内容何时可用、实际 hydrated editor 数量及后台恢复状态。
7. 编译、多个基准不并发执行；同一负载重复多次报告分布。大文件为实际写入日志/Unicode/短行/长行，非稀疏零页吞吐。

## 结果

已完成服务层release实验、原生100标签恢复及普通编辑提交测量。交接文档800ms启动、16ms输入提交、1s首屏及200ms取消仍按各自测量边界解释；没有端到端屏幕延迟、真实IME或NVMe冷盘通过结论。

### 文件尺度与有界内存

查询为不存在的 `__ABSENT_BENCH_NEEDLE__`，全部扫描完整文件、结果未截断。下表扫描块长1 MiB；1 GiB扫描重复3次取中位数，其余扫描单次，不宣称其吞吐分布。首范围每文件51次；每次新 handle、OS缓存未清除。

| 实际写入文件 | 存储首范围 p50 / p95 (µs) | 原始顺序读 (GiB/s) | memchr搜索 (GiB/s) | 字节扫描实验 (GiB/s) | 整次命令 peak RSS (MiB) |
|---|---:|---:|---:|---:|---:|
| 1 GiB 日志 | 29.916 / 37.167 | 16.742 | 11.396 | 1.628 | 18.11 |
| 10 GiB 日志 | 29.459 / 32.958 | 0.850 | 0.852 | 0.851 | 13.59 |
| 64 GiB 日志 | 31.041 / 33.250 | 0.852 | 0.852 | 0.852 | 11.58 |
| 128 MiB 单行 | 24.542 / 26.500 | 0.852 | 9.758 | 1.617 | 11.98 |

64 GiB 大于本机物理内存；该轮三个完整扫描各约75.1秒。RSS是独立服务CLI的 `/usr/bin/time -l` maximum resident set size，不是GUI或操作系统文件缓存。第一轮读取和后续缓存命中不可混成设备速度：128 MiB原始读取和随后搜索差异即明确反例。

### 块长与算法选择

1 GiB日志同轮缓存条件下3次memchr扫描吞吐中位数：

| 块长 | memchr (GiB/s) |
|---|---:|
| 64 KiB | 8.663 |
| 256 KiB | 10.724 |
| 1 MiB | 11.396 |
| 4 MiB | 11.214 |

决定保留默认1 MiB有界分块与memchr；4 MiB未带来本轮收益，字节扫描在缓存命中时明显慢。10/64 GiB三种实现约同速，符合本轮USB存储瓶颈，不能把缓存内11 GiB/s宣传为磁盘吞吐。未实验 mmap、异步I/O和并行分片，不能声称已排除它们。

### 取消与恢复成本

- 1/10/64 GiB搜索运行中取消分别1.514/1.507/1.511 ms，均回到 `active=0`；128 MiB查询已提前结束，不计取消成绩。
- 满结果队列（8批）取消实测10.536–15.053 ms；20次最新查询替换最终只采纳最新查询。这是少量场景样本，不是p95统计。
- 4 MiB草稿20次增量恢复提交：worker总提交 p50 **345.542 µs**、p95 **576.750 µs**；包含重放一致性比较与SQLite FULL。一致性比较累计3.036711 ms。它不是UI打字延迟或物理掉电测试。

原始文件尺度测量：[`evidence/service-size-benchmarks.json`](evidence/service-size-benchmarks.json)。数据生成及实分配字节：[`evidence/fixture-generation.txt`](evidence/fixture-generation.txt)。

### 多文件、密集行与100标签恢复

以下为独立服务CLI单轮结果，不是100个原生编辑器已创建的测量。

| 场景 | 观察 |
|---|---|
| 16个8 MiB文件、并发索引时2048次分页读取 | p50 22.833 µs，p95 49.209 µs |
| 全局分页缓存 | 实际峰值64 MiB，预算64 MiB；索引并发峰值2 |
| 关闭全部16个源 | 缓存0字节、索引0字节、源0个 |
| 多文件完整128 MiB搜索，最多4 worker | memchr 18.363 GiB/s；字节扫描4.226 GiB/s；缓存条件、单次 |
| 多文件命令 peak RSS | 75.66 MiB，包含64 MiB页面缓存 |
| 512 MiB全换行密集文件 | 536,870,913行；索引0.587秒、131,072字节 |
| 100草稿重启清单 | 元数据185 µs；仅活动草稿hydration 221 µs、69,632字节；其余99个未hydrate |
| 64 GiB末尾搜索 | 正确返回 `68719476673..68719476694`；完整扫描68,719,476,736字节 |

密集行索引是稀疏采样而非每行常驻位置数组；行数包括结尾空行。关闭后缓存归零验证存活资源释放，不证明分配器立即归还RSS。纯RAM64 MiB分块复制加扫描的1 MiB块吞吐：memchr12.045 GiB/s、字节扫描1.629 GiB/s，单次样本。

原始证据：[`evidence/service-multifile-benchmarks.json`](evidence/service-multifile-benchmarks.json)。

### 原生100标签启动验证（报告生命周期修复后）

同一个真实100草稿数据库连续3次启动，各自完整输出 `passed=true`、`shutdown_durable_ack_observed=true`。下面是 `/usr/bin/time -l` 包含退出的**整次验证进程**时间，不是屏幕首帧测量。

| 运行 | 整次进程秒 | peak RSS MiB | 标签数 | 已hydrate编辑器 | 已hydrate文本字节 |
|---|---:|---:|---:|---:|---:|
| 1 | 0.36 | 119.73 | 100 | 1 | 69632 |
| 2 | 0.35 | 120.47 | 100 | 1 | 69632 |
| 3 | 0.36 | 119.14 | 100 | 1 | 69632 |

报告里的 `process_to_window_ms` 是窗口构建阶段计时，`first_interactive_ms` 当前在创建初始编辑器后赋值，不是显示帧/真实输入响应探针，不能据此声称219ms首屏。其余99个标签未构造EditorState由运行报告直接记录，不是从RSS除以标签数推算。仅3个样本，不计算p95。

证据：[运行1](evidence/native-100tabs-fixed1.json)、[运行2](evidence/native-100tabs-fixed2.json)、[运行3](evidence/native-100tabs-fixed3.json)、[OS原始输出](evidence/native-100tabs-fixed-os.txt)。

### 普通编辑提交成本

最终原生完整smoke在1,048,640字节普通源码文档上执行128次实际EditorState输入/删除/撤销类操作，恢复服务保持开启；不是假文本缓冲区基准。

| 指标 | 毫秒 |
|---|---:|
| 提交p50 | 0.066708 |
| 提交p95 | 0.071625 |
| 最慢提交 | 0.575042 |
| 单次64 KiB粘贴提交 | 1.483875 |

计时覆盖编辑提交与同步事件处理，不覆盖屏幕呈现、输入法、每次磁盘durable等待或持续真实键盘输入的帧率。128样本来自单次运行，不外推所有语言/文件尺寸；不可将它宣传为0.07ms端到端打字。完整smoke还验证了事务一次撤销、版本拒绝、替换后搜索、保存与重启恢复，见 [native-full.json](evidence/native-full.json)。

## 复现服务实验

```sh
cargo build --locked --release -p editor-services
mkdir -p .bench-data
target/release/editor-services generate .bench-data/one.log 1GiB log
target/release/editor-services bench .bench-data/one.log __ABSENT_BENCH_NEEDLE__ 3 64,256,1024,4096
target/release/editor-services cache-bench .bench-data/cache 16
target/release/editor-services session-bench .bench-data/session 100
target/release/editor-services ram-bench 64MiB
```

64 GiB场景使用相同`generate`命令的`64GiB log`参数，随后`bench ... 1 1024`；会实际写满64 GiB并多次完整扫描，不是快速稀疏占位。不要同时编译、生成其他大文件或运行其他基准。上述命令不清理OS缓存；先注明数据卷接口，再比较吞吐。

本轮未提交工作树，不伪造commit SHA；锁文件、实际被测服务CLI和最终原生应用的SHA-256见 [build-identity.txt](evidence/build-identity.txt)。

显示环境实测：U2790B，逻辑2560×1440，AppKit backingScaleFactor=2.0；见 [display-environment.txt](evidence/display-environment.txt)。显示环境记录不替代逐帧呈现测量。

## 日常编辑功能补齐前后对照

同机、同外置卷、release构建、未清OS缓存。100标签使用同一源SQLite数据库的独立备份；每组3次进程运行，**不是p95样本，也不是逐帧首屏测量**。补齐后默认显示侧栏并加载独立偏好存储。

| 指标 | 补齐前 | 补齐后最终release |
|---|---|---|
| 100标签全进程时间（含退出） | 0.41 / 0.28 / 0.28 s | 0.32 / 0.32 / 0.29 s |
| 峰值RSS | 119,373,824 / 118,964,224 / 119,406,592 B | 121,962,496 / 122,470,400 / 122,273,792 B |
| 已hydrate编辑器 / 内容 | 1 / 69,632 B | 1 / 69,632 B |
| 1 MiB文档128次提交p50 | 0.068542 ms | 0.064625 ms |
| 同组提交p95 | 0.078625 ms | 0.071959 ms |
| 同组最慢提交 | 0.555792 ms | 0.580208 ms |
| 单次64 KiB粘贴提交 | 1.521083 ms | 1.395375 ms |

侧栏没有导致100标签全文加载；RSS中位数增加约2.77 MiB。进程时间中位数从0.28 s到0.32 s，样本太少且缓存未控制，不能据此宣称启动更快或统计上无退化。输入数字仍仅为提交成本，不包含屏幕呈现或每次durable等待；本轮没有新增64 GiB或Windows ARM性能结论。

原始证据：[补齐前OS输出](evidence/usability-100-before-os.txt)、[最终OS输出](evidence/usability-100-final-os.txt)、[补齐前完整smoke](evidence/usability-full-before.json)、[最终独立完整smoke](evidence/usability-full-verifier.json)、[本轮二进制标识](evidence/usability-build-identity.txt)。中途版本的`usability-100-after*`保留为过程证据，不与最终版本混用。

## 标签组：100/1024草稿迁移与重启

同一Mac、同一外置卷，release串行运行，未清OS缓存。每种规模三份相同v1源库的SQLite备份，各先迁移再重启；1024夹具包含真实恢复头与正文，不是空元数据。以下为三次均值，**全进程时间包含退出，不是首帧或p95**。

| 标签数与路径 | 改动前时间 | 最终时间 | 改动前峰值RSS | 最终峰值RSS |
|---|---:|---:|---:|---:|
| 100，v1迁移 | 0.330 s | 0.337 s | 116.61 MiB | 118.57 MiB |
| 100，v2重启 | 0.330 s | 0.343 s | 116.61 MiB | 118.29 MiB |
| 1024，v1迁移 | 0.400 s | 0.463 s | 167.89 MiB | 174.72 MiB |
| 1024，v2重启 | 0.400 s | 0.427 s | 167.89 MiB | 174.17 MiB |

所有运行只hydrate一个编辑器、69632字节正文。1024标签重启峰值RSS均值增加约6.28 MiB；迁移比旧版本启动多约63 ms。三样本且缓存未控制，不能宣称统计上无退化。最终完整smoke输入提交p50为0.064125 ms、p95为0.072 ms，单次64 KiB粘贴提交1.41525 ms；不含屏幕呈现，不代表端到端输入延迟。

最终editor SHA-256为`0d439c7f069065828326b87cbff836143228ef91daf0ece720ec4f32b524c896`；逐次命令、OS时间/RSS及正文加载量见[group-accepted-summary.json](evidence/group-accepted-summary.json)及`group-accepted-*-os.txt`。改动前二进制为`b8e6f545606fa603a3fa698db10fb5bbc9d84a4ed4ecacdcaf603c13f66245f7`，旧二进制与源码提交的对应未独立证明。`group-final-perf-*`测量的是中间版本`42d527...`，不能当作本表最终版本。

新增可复现夹具及真实窗口场景：

```sh
target/release/editor-services session-fixture /tmp/makeanotepad-groups-unique 1000 1
target/release/editor --data-dir /tmp/makeanotepad-groups-unique --smoke=groups --smoke-report /tmp/makeanotepad-groups-report.json
```

夹具命令拒绝已有数据库。1000草稿整组取消/关闭不加载全部正文；关闭后持久状态已验证为1000个Closed加1个新空白。未新增64 GiB扫描、Windows ARM性能、长期内存泄漏或逐帧呈现结论。

## 不同文档分屏：最终同条件矩阵

本机macOS ARM release，串行执行，无同时编译或其他验证窗口。8个标签组，每文档69,632 bytes；`session-fixture DIRECTORY VIEWS 8 PANES`生成1/2/4/8窗格。每格3次startup取中位数，另一次splits场景提交128次输入。OS缓存未清除；不是冷盘或屏幕端到端延迟。窗格就绪来自实际窗口布局及可见正文恢复，不是截图首帧。

| 标签 | 实际可见窗格 | 可见窗格就绪中位数(ms) | startup peak RSS中位数(MiB) | 输入提交p95(ms) |
|---:|---:|---:|---:|---:|
| 100 | 1 | 270.54 | 126.69 | 0.0849 |
| 100 | 2 | 273.70 | 132.55 | 0.0853 |
| 100 | 4 | 280.06 | 139.30 | 0.0830 |
| 100 | 8 | 293.48 | 156.25 | 0.0858 |
| 1024 | 1 | 287.16 | 174.77 | 0.0853 |
| 1024 | 2 | 293.18 | 180.47 | 0.0845 |
| 1024 | 4 | 302.94 | 187.97 | 0.0863 |
| 1024 | 8 | 317.13 | 205.69 | 0.0856 |

全部32次进程成功退出并收到durable ACK。正文hydrate数量严格等于可见窗格数量；1024标签8窗格只加载557,056 bytes正文。额外4窗格限制1个hydrate编辑器时只加载焦点文档69,632 bytes，其余显示预算状态。RSS不是正文预算，表中startup RSS也不是输入场景RSS；1024标签8窗格输入场景peak RSS为231.69 MiB。

原始命令、每次时长/RSS及JSON路径见[evidence/split-final-matrix.json](evidence/split-final-matrix.json)，二进制SHA-256见[evidence/split-final-identity.json](evidence/split-final-identity.json)。复现先创建不存在的夹具目录，再运行`/usr/bin/time -l target/release/editor --data-dir DIRECTORY --smoke=startup --smoke-report REPORT.json`，输入场景换为`--smoke=splits`。预算场景追加`--max-hydrated-editors=1`。

这些数据证明当前恢复/输入路径未随隐藏标签加载全部正文，不证明长期无泄漏、持续帧率、物理输入延迟或Windows性能；未将不同夹具的历史标签组结果作为严格前后对照。


## 同文档分页多视口：隔离原生实验

`spikes/multiview-paged`，macOS ARM release，1440×860逻辑像素窗口，固定同一8 MiB密集文本源（131,017逻辑行）。每种视口数3个独立进程串行运行，无并行编译/其他验证窗口；OS缓存未清除。下表均为3次中位数。

| 共享源视口数 | 同偏移正文加载+绘制(ms) | 不同偏移稳态同步绘制(ms) | 整个实验peak RSS(MiB) |
|---:|---:|---:|---:|
| 1 | 15.046 | 0.600 | 106.67 |
| 2 | 19.430 | 1.169 | 110.31 |
| 4 | 16.378 | 1.301 | 111.23 |
| 8 | 21.292 | 2.197 | 116.30 |

加载计时对所有N只覆盖一次请求至正文可用及绘制；1视口额外的重复读取去重检查不计入该区间。异步就绪轮询也包含在加载计时中。同步绘制指真实GPUI的`window.draw`，不是Metal呈现完成、持续帧率或物理输入延迟。RSS覆盖完整实验：密集数据、索引、搜索、稀疏偏移、源变化和关闭，不是纯稳态视口内存。

12次运行全部通过行为断言。所有配置只有1个共享源；8视口读相同偏移时页缓存仅65,536 bytes，读不同偏移时524,288 bytes；索引完成占2,048 bytes。1视口移动到另一偏移仍保留原缓存页，因此其不同偏移阶段为2页；2/4/8视口分别为2/4/8页。视口自己的分段及布局数据不计入页缓存预算，另记录在每次报告中。关闭至1视口保留源/索引，关闭最后视口并等待任务释放后源、缓存与索引归零。

每次另外使用4 GiB + 4 MiB的稀疏文件验证大于32位的独立视口偏移、continuation及实际gutter格式；不全量扫描，不作为读盘吞吐证据。原始命令、二进制身份、逐次计时和RSS见[evidence/multiview-final-matrix.json](evidence/multiview-final-matrix.json)及[evidence/multiview-final-identity.json](evidence/multiview-final-identity.json)。

普通编辑器实验仅为两份独立缓冲区的反例及IME诊断，**没有共享可编辑文档的性能成绩**。其中观察器包含事件记录及一致性比较，不与主应用输入提交基准混用。


## 单编辑器IME修复后的局部提交与普通文件打开对照

本轮本机macOS 27.2、ARM64、Mac16,10、32 GiB；夹具和恢复目录位于独立`/tmp`，未清除OS缓存（warm/unspecified）。实际release二进制身份见[ime-final-identity.json](evidence/ime-final-identity.json)。这不是与旧版相同工作量的严格前后对照。

[实际工作区报告](evidence/ime-workspace-final.json)依次使用1,048,640 B Rust、16,777,216 B纯文本及67,108,860 B纯文本。后者为64 MiB−4 B，给3 B中文提交留出恢复上限余量，不是64 GiB分页测试。每份32周期，每周期6种原生调用，各操作32样本，总计192次提交、64个已提交revision；最终字节不变、实际持久化完成，并关闭后才打开下一份。计时观察器只计数，不构造全文字符串或哈希。

以下为各操作的提交耗时p95（ms）；完整p50/p95/max保留在原始JSON。包含原生调用及同步事件结算，不是屏幕呈现或物理输入法延迟。

| 操作 | 约1 MiB Rust | 16 MiB纯文本 | 64 MiB−4 B纯文本 |
|---|---:|---:|---:|
| mark `n` | 0.0576 | 0.0603 | 0.0438 |
| mark `ni` | 0.0533 | 0.0640 | 0.0421 |
| 提交`你` | 0.0939 | 0.1211 | 0.0698 |
| Undo | 0.1970 | 0.2172 | 0.1635 |
| mark `x` | 0.0628 | 0.1011 | 0.0477 |
| 空标记取消 | 0.0552 | 0.0460 | 0.0425 |

这576次记录中的最大单次提交为0.5937 ms；样本有限，不承诺任意事务、持续输入帧率或系统候选窗口都低于该值。大跨度事务仍可能需要大跨度比较及完整恢复checkpoint。

**普通文件加载的资源成本仍需收敛。** 独立验证器额外生成相同行形状、同为67,108,860 B的纯文本，只执行`--smoke=startup`打开和正常退出，无编辑；确认1个普通可编辑文档、0个分页文档，见[对照JSON](evidence/ime-open-control-64mib.json)和[原始OS指标](evidence/ime-open-control-64mib-os.txt)。

| 进程场景 | 整次运行秒 | peak RSS MiB | OS peak memory footprint MiB |
|---|---:|---:|---:|
| 单份近64 MiB纯打开/退出 | 2.68 | 695.51 | 768.63 |
| 本地构建：整套IME/生命周期/三规格基准 | 9.65 | 822.76 | 852.97 |
| 下载的CI Mac ARM产物：同IME场景 | 9.93 | 922.31 | 752.64 |

纯打开内部`visible_panes_ready_ms`为2570.38；不是屏幕像素首帧。输入提交快并不代表文件加载与内存已达标。上述为不同进程、不同工作量的独立峰值，不能相减认定IME新增占用；没有堆剖析，不归因于Rope、排版缓存或其他具体对象，也不证明泄漏。下一步优先测量普通加载路径的分配、持有者与阶段耗时，再决定优化方案。

CI下载产物的额外运行见[报告](evidence/ime-ci-macos-native.json)、[OS指标](evidence/ime-ci-macos-native-os.txt)及[构建身份](evidence/ime-ci-macos-build-identity.txt)。它仍在本机执行，夹具位于`/Volumes/macport/Caches/tmp`，与前述本地构建的二进制SHA和存储位置不同。此运行用于验证实际产物，不与前表合并分位数，也不作为托管runner性能数据。

## 普通文件加载优化：相同夹具的旧/新release对照

采用ADR012：新EditorMode直接接管服务层Rope，行布局顺序遍历LF行并流入SumTree，移除初始全文编辑路径和行项暂存数组。版本、普通可编辑上限、恢复/撤销与折行策略不变。以下只代表本机macOS ARM64，不外推Windows或持续屏幕延迟。

### 归因与边界

- [旧版启动采样](evidence/load-sample-before.txt)在`complete_disk_open`的240个活动样本中，222个位于适配器构造，135个经过逐行`slice_line`，22个经过Rope插入；样本数不是精确墙钟占比。
- [直接分配栈节选](evidence/load-allocation-stacks-before.json)来自另一个启用`MallocStackLogging=1`的进程。两个选中的SumTree栈组共95,328次存活分配、122,019,840 B（116.37 MiB）；三个选中的Rope插入栈组共90,108,928 B，含1024/1280 B分配。这是采样时存活栈组，不是完整堆普查或瞬时峰值分解。
- Instruments Allocations尝试受SIP/目标限制而未取得有效结果；改用`sample`与`malloc_history`，未修改SIP或系统安全配置。
- 新版普通后台启动的`vmmap`曾得到148.6 MiB，但缺少已完成布局/绘制的证据；旧版初始化会提前构造行树，新版是延迟准备。因此该数值**不用于前后稳定内存比较**，不宣称常驻内存已降至148.6 MiB。
- [新版已绘制启动采样](evidence/load-sample-after-draw.txt)来自PID 9697的`--smoke=startup`，`sample 9697 3 1`与目标进程均exit 0，[同进程smoke报告](evidence/load-sampled-startup.txt)确认正常退出。1430个主线程样本中878个经过`TextElement::prepaint`，809个经过`TextWrapper::update_wrapped`，434个经过grapheme遍历；顺序行迭代为29个样本。它指出下一热点仍在首次折行，不用不同采样窗口的计数差计算速度收益。

### 同夹具对照

平台为macOS 27.2、Mac16,10、32 GiB。保留旧二进制SHA `9acee6…`，新版为`adde34…`；完整SHA、每次原始smoke报告、OS输出、命令和夹具生成规则见[对照数据](evidence/load-startup-paired.json)。均使用独立`/tmp`夹具/恢复目录，未清除OS缓存，标为warm/unspecified。

两份夹具均为67,108,860 B（64 MiB−4 B，不是64 GiB）。短行夹具为64 B行，1,048,576个换行符；长行形状对照为64 KiB行，1024个换行符，均含尾空逻辑行。短行按旧/新交替各运行3次，长行各1次；各次均为1个普通hydrate编辑器、0个分页标签，并正常退出取得durable ACK。

| 夹具/版本 | 样本数 | 平均正文就绪 ms | 平均场景总耗时 ms | 平均进程peak RSS MiB | 平均退休指令数 十亿 |
|---|---:|---:|---:|---:|---:|
| 短行/旧版 | 3 | 2325.56 | 2433.54 | 690.66 | 45.38 |
| 短行/新版 | 3 | 1429.85 | 1521.88 | 456.16 | 31.14 |
| 长行/旧版 | 1 | 1292.86 | 1392.90 | 517.75 | 26.39 |
| 长行/新版 | 1 | 1223.67 | 1317.17 | 226.78 | 25.37 |

短行本次样本的平均peak RSS下降34.0%，正文就绪下降38.5%，场景总耗时下降37.5%，退休指令数下降31.4%。长行单次对照的peak RSS下降56.2%；不能将两个修改的独立贡献或各个缓冲区峰值从这些进程差值中分解出来，也不承诺任意文件有相同比例。

`visible_panes_ready_ms`定义未变；`total_elapsed_ms`还包括显式原生draw和正常退出确认，降低的不是仅仅一个被提前发布的就绪计时。两者仍不代表像素首帧。B与C合并验证，没有分别宣称单项速度收益。

**仍未达终态：** 密集短行近64 MiB仍需约456 MiB进程峰值、1.43秒正文就绪。每行SumTree项、首次布局/折行及系统图形资源仍有成本；稳定绘制后的堆、长期多文档内存、冷盘和实体屏幕延迟尚未完成本轮验收。后续继续定位已绘制路径，不靠改为只读或取消恢复保护获得数字。

下载的本轮CI Mac ARM产物另在本机运行，近64 MiB启动与IME场景均通过；其OS原始指标见[产物报告](evidence/load-ci-macos-native.json)。启动进程peak RSS为452.78 MiB，整套IME场景为672.73 MiB；二进制、工作量和采样时间不同，不并入旧/新本地均值或分解为IME额外占用。后台无draw证明及误用CLI的排除记录保留在[无效对照说明](evidence/load-excluded-observations.json)。

## 首次折行快路径与行树无重叠替换：ADR013同夹具对照

采用ADR013：全量无宽度复用重排前先释放旧`SumTree<LineItem>`消除双树峰值重叠，对字节长度不超过`2 * wrap_width`的行先以真实整形宽度检查整行容纳性以跳过无谓的grapheme/UAX#14扫描，并复用`WrapScratch`缓冲消除逐行`Vec<usize>`分配。维护文件仍限于`vendor/gpui-base`原三文件，不近似字形宽度，不改动64 MiB可编辑上限、恢复保护或持久化格式。

### 同夹具串行交替对照

平台为macOS 27.2（Darwin 27.2.0）、Apple Silicon Mac16,10、32 GiB内存；OS文件缓存未清空（warm/unspecified）。对比ADR013前的release基线（`adde34b0...`）与集成ADR013+ADR014的新release二进制（`3d409f36...`），每次使用全新`--data-dir`串行执行`--smoke=startup`，均确认`hydrated_editor_count = 1`、`hydrated_content_bytes = 67,108,860`、显式原生draw完成且观察到正常退出durable ACK。原始记录见[ADR013配对基准](evidence/adr013-startup-paired.json)。

| 夹具形态 / 版本 | 样本数 | 平均正文就绪 ms | 平均场景总耗时 ms | 平均进程peak RSS MiB | 平均peak footprint MiB | 平均退休指令数 十亿 |
|---|---:|---:|---:|---:|---:|---:|
| 重复短行 `short_repeated` / 旧版 | 3 | 1416.78 | 1512.57 | 455.73 | 443.28 | 31.10 |
| 重复短行 `short_repeated` / 新版 | 3 | 434.76 | 526.49 | 331.32 | 325.58 | 5.53 |
| 全唯一短行 `short_unique` / 旧版 | 1 | 28582.15 | 28679.98 | 5305.00 | 5257.73 | 588.57 |
| 全唯一短行 `short_unique` / 新版 | 1 | 7383.48 | 7481.50 | 2324.94 | 2172.61 | 152.84 |
| 64 KiB长行 `long_64k` / 旧版 | 1 | 1228.93 | 1325.19 | 226.66 | 224.13 | 25.40 |
| 64 KiB长行 `long_64k` / 新版 | 1 | 1224.21 | 1320.36 | 226.31 | 223.84 | 25.27 |

- **重复短行（64 B周期行，各3次交替）**：平均peak RSS再降124.41 MiB（-27.3%，较ADR012前690.66 MiB累计下降52.0%），正文就绪下降69.3%至434.76 ms，含显式draw与durable退出的总耗时下降65.2%至526.49 ms，退休指令数下降82.2%。
- **全唯一短行（1,048,575行唯一64 B ASCII + 60 B尾行，单次对照）**：用于暴露宽度缓存未命中上界。peak RSS从5305.00 MiB（5.18 GiB）降至2324.94 MiB（2.27 GiB，-56.2%），正文就绪从28.58秒降至7.38秒（-74.2%），退休指令数下降74.0%。
- **64 KiB长行（1,024行，单次对照）**：超出`2 * wrap_width`阈值，按预期走原有折行路径，内存与耗时持平（差异在0.5%以内），无回退。

### 采样热点与剩余边界

- [ADR013绘制后启动采样](evidence/adr013-sample-after-draw.txt)绑定PID 28484（`short_repeated`夹具，[同进程报告](evidence/adr013-sampled-startup.json)确认exit 0与durable ACK）：主线程共613个样本，其中AppKit首次加载`+[NSTextInputContext initialize]`占233个样本，`TextElement::prepaint`降至124个样本（`DisplayMap::on_layout_changed` 78个、`ensure_prepared_for_layout`初始行树构建45个），`grapheme_indices`与`unicode_linebreak`均为0样本；`update_wrapped`内部剩余样本主要集中在`ropey::Lines`分片读取、宽度表哈希查找及`SumTree`节点分配。采样运行的计时不并入上方未插桩均值。
- **剩余风险**：全唯一百万短行在首次`prepaint`仍同步对全部1,048,576行执行`TextSystem::shape_line`（底层CoreText `CTLineCreateWithAttributedString`），即使`WidthMeasurements`本体限制在32,768条/4 MiB，系统字体整形与百万行`SumTree`仍带来约2.27 GiB峰值与7.38秒首屏等待。后续需评估视口优先或分块渐进折行，以及多文档长期驻留堆分布；上述数据仍仅代表本机macOS ARM64 warm/unspecified状态，不代表冷盘、物理屏幕像素首帧或Windows/Linux性能。
