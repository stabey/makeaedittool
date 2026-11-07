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
