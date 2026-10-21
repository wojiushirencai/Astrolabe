# find_references：索引优先的召回测量（第 6 批，R 路）

原则：语言服务器内存大，只作兜底；语法/图索引是主路径。切换前，在 `/workspace/cc-ab/v2/src` 的语料副本上，对会话里实际查过的符号（外加按固定步长抽样、全仓唯一定义的非测试符号）分别跑
语言服务器（`find_references` 原路径）和索引答案（`refs_graph::graph_references`），按 `(文件, 行, 列)` 比较。
复现：`REFS_RECALL_REPO=<repo> REFS_RECALL_CASES=path:sym,… REFS_RECALL_SAMPLE='.go:40' cargo test -p astrolabe-mcp --lib recall_harness -- --ignored --nocapture`。

索引答案只在 **同语言族里该名字全仓只有一处定义** 时给出，否则交给语言服务器（表中"交给 LSP"）。
Go 的范围：定义所在包（目录）+ 经 import 边到达该包的文件；`fmt.X` 这类经其他包导入名的选择子不算；排除注释和字符串。

| 语料 / 语言 | 用例 | 索引作答 | 交给 LSP | 与 LSP 完全一致 | 召回 | 精度 | LSP 基线状态 |
|---|---|---|---|---|---|---|---|
| cobra / Go（会话 8 个 + 抽样 25） | 33 | 32 | 1 | 28 | **1.000** | 0.996（1020 中多 4） | gopls Exact |
| aiproxy / Go（会话 14 个 + 抽样 40） | 54 | 51 | 3 | 50 | **1.000** | 0.999（1159 中多 1） | gopls Exact |
| aicon / Go（会话 2 个 + 抽样 30） | 32 | 31 | 1 | 31 | **1.000** | 1.000 | gopls Exact |
| aiproxy web / TS（抽样 40） | 40 | 40 | 0 | 31 | **待重测**（TODO(B7-TS): 填入修正后的召回） | **待重测**（TODO(B7-TS): 填入修正后的精度） | 27/40 不是 Exact（无 node_modules，tsserver 退回文本）；原值作废，见下方说明 |
| express / JS（抽样 30） | 30 | 30 | 0 | 13 | 0.958 | 0.535；只看 Exact 的 23 个：0.952 / 0.702 | 7/30 Unknown/Scoped |
| serena / Python（抽样 40，600 s 内跑完 19） | 19 | 19 | 0 | — | **待重测**（TODO(B7-PY): 填入修正后的召回） | **待重测**（TODO(B7-PY): 填入修正后的精度） | pyright 每次 30 s 超时，基线全是文本兜底；原值作废，见下方说明 |

> **更正（第 7 批）：** 本表第 6 批给出的 TS 召回 1.000 和 Python 召回 0.779（约 0.78）不可信，已撤下，正在第 7 批用修好的基线重测。原因是对照组本身是坏的，偏差两个方向都有：
> - **TS 召回 1.000 是高估。** tsserver 缺 node_modules，退回了文本搜索。例如 `IconDiscord` 真实有 6 处引用，索引只给出 2 处（经 barrel 文件 `@/assets/brand-icons` 导入的调用方没找到），退化的 tsserver 只给 1 处，于是这一行被算成"召回 1.0"。TS 精度 0.810 那一栏里的"多报"也大多是 tsserver 漏掉的真实引用。
> - **Python 召回 0.78 是低估。** pyright 每次 30 s 超时，基线是文本搜索。列出来的"漏报"大多是 docstring、f-string 里的文字，或者同名的其他属性和局部变量。真正的漏报主要是字符串类型注解（前向引用），例如 `tools_base.py:53` 的 `"LanguageServerSymbolRetriever"`。
> - JS（express）的基线有 7/30 不是 Exact，数字同样只能参考。
>
> 新基线的做法：TS 装好 node_modules，让 tsserver 达到 Exact；pyright 超时调到 180 s 以上并先预热；召回和精度只用 `Confidence::Exact` 的行计算。修正后的数字由第 7 批的 TS 和 PY 两路补入，见上表 `TODO(B7-TS)` 和 `TODO(B7-PY)`。审计依据：`lang-graph` 审计第 3.1 节。

会话里查过的 Go 符号：cobra 8 个中 7 个索引作答（`MarkFlagRequired` 有多处定义，交给 LSP），aiproxy 14 个中 11 个（`Provider`、`MaskSensitiveInfo`、`Register` 交给 LSP），aicon 2 个中 1 个；作答的全部召回 1.0。

Go 多出的 5 处：
- `cobra.go:186 t.Execute(w, data)`：`text/template` 的 `Execute`，经局部变量调用，语法上分不出。这是唯一一处真误报。
- `command_win.go` 两处：`//go:build windows` 文件，gopls 在 Linux 上不加载。这两处是真实引用。
- aiproxy `responses_grounding_stream.go:75 sharedresponses.BuildResponsesURLCitationsStrict`：经别名导入，gopls 漏了，我们给出的是对的。

时间：索引答案每次 1–350 ms，不起进程。gopls 首次调用 0.45–2.3 s（冷启动），之后 10–300 ms。

## 结论

- **Go：索引优先**（`refs_graph::graph_first_language`）。只有名字有多处定义、或用 `dry_run_rename` 要编译器级清单时，才启动 gopls。
- **TS / JS / Python：保留语言服务器优先**。
  - 基线本身不可靠，无法证明索引不差：tsserver 缺依赖，pyright 超时。
  - 已经确认的真实缺口：TS/JS 经 barrel 文件转导出的引用、改名绑定（default import、CommonJS `require` 改名、`{A as B}`），以及 Python 的字符串类型注解和 `__init__.py` 转导出，索引目前拿不到。召回的具体数字待第 7 批重测（TODO(B7-TS) / TODO(B7-PY)）；第 6 批写的"Python 召回 0.78"是坏基线下的数字，已作废。
  - 这些语言等基线修好后再测。
