# 贡献指南

先读 `docs/ARCHITECTURE.md`。这里的门禁和阈值是测量过的契约，不是风格偏好。数字下降时查原因；不要改阈值让测试通过。

## 开发环境

- **MSRV：** workspace `rust-version = "1.90"`。这是下限。
- **实际钉死的工具链：** `rust-toolchain.toml` 为 **1.96.1**，带 `rustfmt` 和 `clippy`。不要改成 `stable` 浮动通道——fmt/clippy 诊断随六周一发的稳定版漂移，`cargo fmt --check` 和 `clippy -D warnings` 会在不同机器、不同周对打。
- **构建：** `cargo build --workspace`。发布与基准必须 `--release`。debug 的 RSS / 耗时没有产品意义。
- **运行 MCP：** `cargo run -p astrolabe-mcp -- /path/to/repo`，或 `ASTROLABE_ROOT=...`。日志在 stderr，stdout 是 JSON-RPC。

依赖版本只加在根 `Cargo.toml` 的 `[workspace.dependencies]`，成员 crate 用 `xxx.workspace = true`。不要在成员里另写版本号。

## 语料

五语言验收不进 git（根 `.gitignore` 的 `/corpus`）。缺语料时相关 `#[ignore]` 测试不会在 `cargo test --workspace` 里跑，**绿不代表验收过**。

| 语言 | 语料 | 本地路径 | 获取 |
|---|---|---|---|
| Go | gin `v1.12.0` | `corpus/go` | `git clone --depth 1 --branch v1.12.0 https://github.com/gin-gonic/gin.git corpus/go` |
| Java | gson `gson-parent-2.14.0` | `corpus/java` | `git clone --depth 1 --branch gson-parent-2.14.0 https://github.com/google/gson.git corpus/java` |
| Rust | ripgrep `15.2.0` | `corpus/rust` | `git clone --depth 1 --branch 15.2.0 https://github.com/BurntSushi/ripgrep.git corpus/rust` |
| Python | serena | 默认 workspace **同级** `../serena` | 自行检出；可用 `ASTROLABE_CORPUS_PYTHON` 覆盖 |
| TypeScript | openvisio-oss | 默认 workspace **同级** `../openvisio-oss` | 自行检出；可用 `ASTROLABE_CORPUS_TYPESCRIPT` 覆盖 |

也接受 vendored 路径 `corpus/python`、`corpus/typescript`（`import_resolution.rs` 的 `locate_corpus`：环境变量 → `corpus/<dir>` → 同级目录）。

性能基准的语料表见 `docs/PERFORMANCE.md` / `scripts/bench.sh`。五个都缺时 `bench.sh` 仍退出 1，避免空跑绿灯。

## 质量门禁

本地改核心路径时按这个清单跑。CI 默认**不会**替你跑五语言验收和性能门禁。

| 命令 | 作用 | CI（push/PR） |
|---|---|---|
| `cargo test --workspace` | 单元 / 集成测试（跳过 `#[ignore]`） | 是 |
| `cargo clippy --workspace --all-targets -- -D warnings` | 警告即失败 | 是 |
| `cargo fmt --all --check` | 格式 | 是 |
| `cargo deny check` | 许可证、advisory、未知来源、wildcard | 是 |
| `bash scripts/verify.sh` | 五语言仓库内 import 解析验收 | 否（需语料） |
| `bash scripts/bench.sh --ci` | 相对 `examples/baselines/*.json` 的 RSS/耗时/符号数 | 否 |
| `cargo test -p astrolabe-core -- --ignored` | 语料测试（导入 + 符号召回 + 端到端建图） | 仅 `workflow_dispatch` 的 Go/Java/Rust 子集 |

`cargo test --workspace` 的个数会随并行改动变，不要把 README 或 `rust-toolchain.toml` 注释里的「N 个测试」当契约。契约是命令退出码。

`verify.sh` 对任一门语言非 `PASS` 或 cargo 非零即失败。它打印 `ASTROLABE_RESULT|语言|语料|总数|解析数|率|PASS/FAIL`。

符号召回是 `parse/mod.rs` 里带 `#[ignore]` 的 `*_symbol_recall`，行格式 `ASTROLABE_SYMBOL_RECALL|...`。

## 验收契约（不可降低）

### 导入解析：五语言 100%

分母 3012 条仓库内 import（语法提取，不是行扫描）：

| 语言 | 语料 | 分母 | 契约 |
|---|---|---:|---:|
| Python | serena | 1577 | 100% |
| Go | gin | 31 | 100% |
| Java | gson | 984 | 100% |
| Rust | ripgrep | 209 | 100% |
| TypeScript | openvisio-oss | 211 | 100% |

口径：注释与字符串已掩码；PEP 420 命名空间包计入；Rust `use` **按语句**计（`use a::{b, c}` 是一条，任一展开路径指向工作区 crate 即计入）。早期行扫描数字 1557 / 91 / 205 已废弃——会把 docstring 里的示例 import 算进去，又漏掉命名空间包。

实现细节：`import_resolution.rs` 里 Python 的 `threshold` 目前是 **0.95**，其余为 `1.0`。那是测试里偏松的一条，**不是**契约。契约以 README 与 `resolvers/mod.rs` 的表为准：五门 100%。解析率下降必须查清（新语法、排除规则、语料漂移、误判二进制），不许把 0.95 改成 0.90，也不许把 1.0 改成 0.99 来换绿灯。

### 符号召回：每语言 ≥90%（对标独立定义扫描 / 语言服务器文档符号）

README 记录值：Python 100.00%、Rust 99.41%、Go 99.29%、Java 96.51%、TypeScript 93.97%。代码门槛 `RECALL_THRESHOLD = 0.90`。数字掉到门槛以下先看漏报样例；已知残差写在 `parse/mod.rs`，不要靠缩小分母消除它们。

### 性能

阈值、何时允许 `--save` 基线、冷启动为什么不能进门禁：见 `docs/PERFORMANCE.md`。不允许的行为包括：数字漂了就把 ratio/floor 放宽、把 `cargo run` 父进程 RSS 写进基线、用 debug 或 `--in-process` 累计高水位覆盖 spawn 基线。

## Ground truth 的铁律

**分母必须独立于被测代码生成。** 先建分母，再跑解析器。解析失败仍计漏报。读失败写入哨兵名（符号召回里是 `<unreadable>`），解析器对不上就算漏。

反面教材：曾经 **209/209 全绿**，却丢了一整个源文件。那个文件被误判为二进制，它的**三条 import 连同分母一起消失**。测试在问「被测代码承认的那些 import 是否都解析了」，于是全绿。

因此：

- 验收测试自己走文件系统 + 语法提取，**不要**问 `ResolverSet`「这是不是仓库内 import」，也不要复用 `scan::looks_binary` 来决定分母里有没有这个文件。
- 生产扫描可以对已知源码扩展名放宽 NUL 规则（`mcp/src/adapter.ts` 在 8 KB 里有两个 NUL，当二进制会丢掉符号，并让别处三条 import 无法解析）。那是扫描器的行为，不是缩小 GT 的理由。
- 改抽取规则时先看分母是否变了。分母变小而通过率上升，优先怀疑漏文件，而不是「解析变好了」。

## 新增一门语言

按这个顺序，不要先加 MCP 工具。

1. **`Language` + `from_path`。** 扩展名映射是扫描和解析的入口。
2. **workspace 依赖里钉 tree-sitter 语法**，确认与 `tree-sitter 0.27` ABI 一致（现有七个语法已验证同 ABI）。
3. **`parse/queries/<lang>/{symbols,imports,calls}.scm`。** 捕获名跟现有约定（`@name`、`@definition.*`、`@import`、`@reference.call`）。加单元测试：query 能在对应语法上编译。
4. **`ParserPool` 的 `LANGUAGES` 列表。**
5. **`resolvers/<lang>.rs` 实现 `ModuleResolver`。** 只读该语言的构建配置；禁止调用其他 resolver。在 `resolvers::all()` 注册。`detect` 可以读盘，`resolve` 必须纯。panic 由 `ResolverSet` 隔离，不要依赖这一点代替错误处理。
6. **语料：** 选定一个真实仓库和 tag，写入本文的语料表、`verify.sh`、`bench.sh`（若要进性能门禁）、以及 `import_resolution.rs` / 符号召回的 `corpus()`。
7. **验收：** 导入解析契约（新语言也是 100%，除非你能说明为什么这门语言做不到——那是架构讨论，不是把阈值写成 80% 先合并）。符号召回 ≥90%。
8. **文档：** 在 `resolvers/mod.rs` 的表和 README 验收契约里加一行。不要只改测试。

调用边、LSP、改写不是加语言的前置。没有构建配置可读的语言，import 图会停在先前方案那种个位数百分比——那不叫支持该语言。

## 提交信息

现有历史是英文祈使句，一条说清做了什么、为什么值得存在：

```
Initial commit: file-level code graph engine with MCP server
Wire store/cache/watch into the pipeline; add Phase 2/3 contracts
```

请保持：

- 祈使语气、现在时（Wire / Add / Fix / Bound），不要 `Fixed` / `WIP` / `update`。
- 不强制 `feat:` / `fix:` 前缀；现有历史没用 Conventional Commits。
- 标题一行；需要「为什么」时在正文写（例如「FileId 随文件集分配，所以只持久化解析缓存」）。
- 不要在提交里改验收阈值、deny allow-list 或 bench ratio，除非提交正文写明对应的产品原因，并附 `verify.sh` / `bench.sh` 输出。

版本号与 tag 约定见 `.github/RELEASING.md`。发布时需要 `CHANGELOG.md` 里有对应小节。

## 改代码时的额外约束

- **`types.rs` 是并行工作流的契约。** 改公共类型前先确认调用方；不要为了一个 resolver 的方便把 `ModuleResolver::resolve` 改成可写磁盘。
- **确定性：** 同输入字节必须得到同图、同排序。并行 parse 之后按路径排序再分配 `FileId`。PageRank 固定轮数、顺序累加。
- **工具响应：** `content[0]` 必须是非空文本。只返回 `structuredContent` 会在 Cursor 里被静默丢弃。
- **不要常驻 LSP、不要动态回退、不要静默吞解析失败。** 理由和数字在 `docs/ARCHITECTURE.md`。
