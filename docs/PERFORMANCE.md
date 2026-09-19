# Astrolabe 索引性能：测量方法、基线与回归门禁

核心卖点是「内存可控 + 速度快」。本文件把那组数字从手工命令变成可复现、可对比、可失败退出的回归门禁。

**测的是 `index_repo()` 在独立 worker 进程里的代价**（`crates/astrolabe-core` 的 `index_bench` 示例），不是 MCP server 常驻进程，也不含 `cargo run` 父进程。调用边默认关闭（`--calls` 才打开）。

## 怎么跑

```bash
# 一键五语料对照表。缺语料会 SKIP 并说明原因，不会崩。
bash scripts/bench.sh

# CI 门禁：相对 examples/baselines/*.json 超阈值则非零退出
bash scripts/bench.sh --ci

# 确认无误后把本次中位数写成新基线（见下文「何时允许更新」）
bash scripts/bench.sh --save

# 单仓、JSON、对比上次
cargo run --release --example index_bench -- \
  --warmup 2 --runs 5 --format json \
  --baseline crates/astrolabe-core/examples/baselines/serena.json \
  --save target/bench/serena.json \
  ../serena
```

必须 **release**。debug 构建的 RSS / 耗时没有产品意义。

语料路径（相对仓库根）：

| 名称 | 路径 | 说明 | 缺失时 |
|---|---|---|---|
| `go` | `corpus/go` | gin | SKIP，打印原因 |
| `java` | `corpus/java` | gson | 同上 |
| `rust` | `corpus/rust` | ripgrep | 同上 |
| `serena` | `../serena` | 工作区外 | 同上 |
| `openvisio-oss` | `../openvisio-oss` | 工作区外 | 同上 |

五个都缺时 `bench.sh` 仍返回 1，避免 CI 空跑绿灯。

机器可读输出：

- JSON（`--format json` / `--save`）：`schema: astrolabe-bench/v1`
- 一行 `ASTROLABE_BENCH|name|scanned|symbols|fails|unresolved|elapsed_us|min_us|max_us|peak_rss|min_rss|max_rss|VERDICT`

`VERDICT`：`PASS` / `REGRESS` / `STALE_BASELINE` / `NO_BASELINE`。`--ci` 对前两个失败项退出 1。

## 测量口径

### 预热 + 中位数

默认 **2 次预热丢弃 + 5 次实测取中位数**。基线文件里的数字来自 2+7，方便看抖动。

每个实测 run 都是一次 **新进程**（`--in-process` 才在同进程循环）。原因：`getrusage.ru_maxrss` / `VmHWM` 是进程生命周期高水位，同进程连跑只会单调上升，中位数没有意义。

Worker 只给 stdout 打 JSON；进度在 stderr。`--save` 在打印之前落盘，避免管道提前关闭丢文件。

### 峰值 RSS（不加依赖）

| 平台 | 峰值 | 持有期 RSS |
|---|---|---|
| Linux | `/proc/self/status` 的 `VmHWM`（kB→字节） | `VmRSS` |
| macOS | `mach_task_basic_info.resident_size_max`，并与 `getrusage(RUSAGE_SELF).ru_maxrss`（**Darwin 单位是字节**）取较大值 | `resident_size` |
| 其他 | 报 `unsupported`，RSS 字段为 0；耗时和计数仍可用。不要在这种平台上开 RSS 门禁。 | — |

另有 5ms 采样线程记录持有图谱期间的 `current_rss` 高点，与内核 HWM 取 max。本机三次实测里两者始终相等：峰值出现在图谱持有期，而不是解析瞬间的尖峰。

本机交叉验证（serena，`/usr/bin/time -l`）：

- `maximum resident set size` = 31 277 056（29.8 MB）
- `index_bench` 报 31 260 672（29.8 MB）
- `peak memory footprint` = 28.4 MB（Activity Monitor 的 Memory 列，略低）

门禁用 RSS 高水位，与 `time -l` 的 maximum RSS 对齐。

### 和 README 那张对照表不是同一口径

README 写 serena **73 MB / 1.0s / 10075 符号**。本机用上述口径，稳态中位数是 **~30 MB / 90–160 ms / ~10050 符号**。

差额来自口径，不是这次「变快了 10 倍」的产品承诺：

1. **进程边界**。`cargo run --example` 的 RSS 含 cargo 父进程。本 bench 只计 example 二进制。
2. **冷启动 vs 预热后**。丢弃的第一次 worker 会把 tree-sitter 语法、忽略规则树、页缓存一起算进去。本机见过 rust 预热 64 MB、openvisio-oss **75 MB / 1.3s**，随后稳态分别落到 20 MB / 14 MB。门禁必须看稳态，否则冷/热 CI 会对打。
3. **对象**。MCP 常驻（tokio + 工具循环）会高于一次 `index_repo`。持久化 / 有界缓存接到编排层之后，应另开一列，不要和这条基线混用。
4. **并发改动**。测量窗口里别的任务在改 `src/index.rs`、`store.rs`、解析器；符号数从 10090 漂到 10051。以基线 JSON 里那一次为准。

README 的 25 / 27 / 55 / 67 / 73 MB 保留为**对外叙事**。回归门禁以 `examples/baselines/*.json` 为准。

## 本机实测

- 机器：Apple M4，10 逻辑核，32 GB，macOS 25.6.0（arm64）
- 工具链：rustc 1.96.1，`profile.release`：opt-level=3，thin LTO，codegen-units=1
- git：`a66eef580d450bc8e16195d623fee6dba23bb709`，**工作区脏**
- 二进制 mtime：2026-09-11 17:30:24 CST（第三次、写入基线的那次）
- 调用边：关

三次独立会话（同一台机器、同一组语料；第 1 次 1 预热+7 实测，第 2、3 次 2 预热+7 实测）。第 3 次与其它 cargo 任务抢机器，耗时被抬高，**RSS 几乎不动**。

### 中位数对照

| 语料 | 扫描 / 解析 | 符号 | 导入边 | 失败 | RSS 三次中位（MB） | 耗时三次中位 |
|---|---:|---:|---:|---:|---:|---:|
| go（gin） | 130 / 99 | 2482 | 31 | 0 | 14.30 / 15.4 / **15.20** | 37.5 / 39 / 51.1 ms |
| java（gson） | 312 / 264 | 5372 | 1022 | 0 | 22.17 / 22.8 / **22.22** | 63.5 / 72 / 67.3 ms |
| rust（ripgrep） | 229 / 110 | 4728 | 157 | 0 | 19.98 / 19.9 / **20.22** | 41.8 / 42 / 42.2 ms |
| serena | 1028 / 492 | 10090→**10051** | 1532→1529 | 0 | 30.19 / 30.2 / **30.22** | 88.4 / 91 / 159.9 ms |
| openvisio-oss | 193 / 154 | 2234 | 280 | 0 | 13.89 / 13.7 / **13.61** | 38.9 / 39 / 39.8 ms |

粗体是写入 `examples/baselines/` 的第 3 次。

### 单次会话内抖动（第 3 次，7 个样本）

| 语料 | RSS min–max（MB） | RSS 极差/中位 | 耗时 min–max | 耗时极差/中位 |
|---|---|---:|---|---:|
| go | 14.69–15.53 | **5.5%** | 40.6–76.5 ms | 70% |
| java | 21.22–22.88 | **7.5%** | 62.3–94.1 ms | 47% |
| rust | 19.75–20.39 | **3.2%** | 40.3–46.2 ms | 14% |
| serena | 29.98–31.03 | **3.5%** | 89.8–246.5 ms | 98% |
| openvisio-oss | 13.50–15.12 | **11.9%** | 38.5–41.2 ms | 6.6% |

跨会话 RSS 中位数变化 ≤ 1 MB。耗时在空闲时约 10–20%，机器繁忙时 serena 中位数可以从 90 ms 到 160 ms（约 1.8×），单样本能到 247 ms。

第 2 次会话里，**1 次预热不够**：rust 第一个实测 63 ms（随后稳定在 41–43 ms），serena 第一个实测 109 ms。所以默认预热是 2。

### 冷启动（已丢弃，不要当基线）

第 2 次会话第一次 worker：go 26.8 MB / 181 ms，java 35.8 MB，rust **63.8 MB**，openvisio-oss **75.1 MB / 1306 ms**。这更接近 README 量级，但不可复现（取决于页缓存和是否刚链完二进制），不能做门禁。

## 阈值

失败条件（中位数 vs 基线中位数）：

```
current > max(baseline × ratio, baseline + floor)
```

外加：

- `parse_failures` 上升 → `REGRESS`
- `symbols` 低于基线 99% → `REGRESS`（质量，不是性能）
- `files_scanned` 与基线不同 → `STALE_BASELINE`（语料变了，先更新基线）

默认（可用环境变量覆盖）：

| 量 | ratio | floor | 环境变量 |
|---|---:|---:|---|
| 峰值 RSS | **1.20** | 4 MiB | `ASTROLABE_BENCH_RSS_RATIO` / `ASTROLABE_BENCH_RSS_FLOOR_BYTES` |
| 耗时 | **2.50** | 150 ms | `ASTROLABE_BENCH_TIME_RATIO` / `ASTROLABE_BENCH_TIME_FLOOR_MS` |

### 为什么是这组数

**RSS 1.20 × 且至少 +4 MB。** 同机同二进制，会话内极差 3–12%，跨会话中位 ≤ 1 MB。20% 大约是观测抖动的 2–4 倍，能吃掉分配器/ASLR，挡得住「缓存没封顶」这种翻倍。4 MB 地板让 14 MB 的 go 不会因为 2 MB 噪声误报；对 serena（30 MB）真正起作用的是 20%（上限 **36.3 MB**）。

太紧（例如 5%）会在 macOS 页对齐上误报。太松（例如 2×）会放过持久化误接到热路径导致的常驻翻倍。

**耗时 2.50 × 且至少 +150 ms。** 空闲时 10–28% 足够 1.5×；但本机在并发 `cargo` 下 serena 中位数到过 1.8×，单样本 ~2.7×。1.75× 会把「开发机上和别人一起编」当成回归。2.50× 让 90 ms 基线可以到 225 ms、160 ms 基线可以到 **400 ms**。150 ms 地板覆盖 go/java/rust 这种 40–70 ms 的小仓（绝对噪声和索引时间一个量级）。这挡的是灾难（秒级、十秒级），不是 20% 的微回归——那件事 RSS 更灵敏。

按第 3 次基线，门禁上限：

| 语料 | RSS 上限 | 耗时上限 |
|---|---:|---:|
| go | 19.2 MB | 201 ms |
| java | 26.7 MB | 217 ms |
| rust | 24.3 MB | 192 ms |
| serena | 36.3 MB | 400 ms |
| openvisio-oss | 17.6 MB | 190 ms |

**换机器不要直接用这份 JSON。** 先在目标机跑 `bash scripts/bench.sh --save`，把新基线提交到该 runner 的配置，或按 runner 标签分文件。M4 的 30 MB 不能拿去卡一台共享 CI 的 x86 虚机。

## 何时允许更新基线

允许（PR 必须写明原因，并贴 `scripts/bench.sh` 新表）：

1. **有意的功能**接到了索引热路径：持久化 mmap、有界缓存、新语言、默认打开调用边，RSS/耗时的增长与设计相符。
2. **语料升级**（`files_scanned` 变了会 `STALE_BASELINE`）。先确认扫描规则不是回归，再 `--save`。
3. **工具链 / 平台**明显不同（新 rustc、换 CI 镜像），且新旧对比已说明。
4. **口径变更**（改了 RSS 数据源）。旧数字作废，重测。

不允许：

- 「数字漂了，把门放宽」而没有对应 diff
- 把冷启动或 `cargo run` 父进程的 RSS 写进基线
- 为让 CI 变绿而只改 ratio/floor、不改代码也不解释
- 用 debug 构建或 `--in-process` 的累计 HWM 覆盖 spawn 基线

流程：空闲机器、release、`bash scripts/bench.sh --save`，审查 `git diff crates/astrolabe-core/examples/baselines/`，RSS 跳变应能在 diff 里对上。

## `index_bench` 能力摘要

- `--warmup` / `--runs`，中位数 + min/max + 极差百分比
- 每 run 独立进程；`--once` 给 harness 用；`--in-process` 仅调试（RSS 不可比）
- 峰值 RSS，无新 crate：Linux procfs，macOS mach + `getrusage`
- `--format human|json|kv`，`--save` / `--baseline` / `--ci`
- 相对门禁：`max(ratio, floor)`；失败计数与符号数质量门
- `--calls` 与旧示例兼容；`ASTROLABE_HOLD=1` 仍可把图谱挂住 6 秒方便外接采样

## 复现清单

1. `cargo build --release -p astrolabe-core --example index_bench`
2. 五个语料都在（`corpus/` 未进 git，需自备；serena / openvisio-oss 在上一级目录）
3. 尽量空闲；记下 `git rev-parse HEAD` 和是否脏
4. `bash scripts/bench.sh --ci`
5. 结果 JSON 在 `target/bench/`；对照 `crates/astrolabe-core/examples/baselines/`
