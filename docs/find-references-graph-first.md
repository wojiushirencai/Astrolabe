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
| serena / Python（抽样 40，600 s 内跑完 19） | 19 | 19 | 0 | 17 | 0.936（第 7 批改后 **1.000**） | 1.000（改后 1.000） | 第 7 批用 pyright 重测：预热、不设超时、无文本兜底。原值 0.779 是第 6 批坏基线下的数字，已作废；详见文末“第 7 批：Python 重测” |

> **更正（第 7 批）：** 本表第 6 批给出的 TS 召回 1.000 和 Python 召回 0.779（约 0.78）不可信，已撤下，正在第 7 批用修好的基线重测。原因是对照组本身是坏的，偏差两个方向都有：
> - **TS 召回 1.000 是高估。** tsserver 缺 node_modules，退回了文本搜索。例如 `IconDiscord` 真实有 6 处引用，索引只给出 2 处（经 barrel 文件 `@/assets/brand-icons` 导入的调用方没找到），退化的 tsserver 只给 1 处，于是这一行被算成"召回 1.0"。TS 精度 0.810 那一栏里的"多报"也大多是 tsserver 漏掉的真实引用。
> - **Python 召回 0.78 是低估。** pyright 每次 30 s 超时，基线是文本搜索。列出来的"漏报"大多是 docstring、f-string 里的文字，或者同名的其他属性和局部变量。真正的漏报主要是字符串类型注解（前向引用），例如 `tools_base.py:53` 的 `"LanguageServerSymbolRetriever"`。第 7 批用 pyright 重测，同一批 19 例在第 6 批规则下召回 0.936、精度 1.000（见文末）。
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
  - 已经确认的真实缺口：TS/JS 经 barrel 文件转导出的引用、改名绑定（default import、CommonJS `require` 改名、`{A as B}`），以及 Python 的字符串类型注解和 `__init__.py` 转导出，索引目前拿不到。TS 的数字见 TODO(B7-TS)；Python 已重测，见文末“第 7 批：Python 重测”。第 6 批写的"Python 召回 0.78"是坏基线下的数字，已作废。
  - 这些语言等基线修好后再测。

## 第 7 批：TS/JS 重测（可信基线）

第 6 批 TS 一行的对照组是坏的（tsserver 缺依赖，27/40 退回文本），"召回 1.000" 不成立。本次换成可信基线：
- **基线：** TypeScript LanguageService `findReferences`，从声明位置查询（TypeScript 5.9.3）。
  - aiproxy web 先装好 node_modules，按 `tsconfig.app.json` 建程序，"Cannot find module" 为 0。
  - trpc（pnpm workspace，7 个包，634 个文件）去掉 examples/www 后装依赖，用根 `tsconfig.json` 的 `paths`（`@trpc/*` 指向各包 src）。"Cannot find module" 只有 2 处，都是测试里的 `aws-lambda` 类型。
  - express 用 `allowJs` 加载全部 141 个 .js。
- **用例：** 第 6 批的同一批用例（TS 40、JS 30），外加抽样：aiproxy TS 200 例；trpc 顶层 200 例；trpc 成员 120 例（`Method,Field`）；express 59 例（JS 抽样池只有这么多）。抽样都是全仓唯一定义、非测试文件。
- **三档对比：**
  - 第 6 批：定义文件 + 直接 importer。
  - 7a：加转导出链和改名绑定（451c50e）。
  - 7b：再加成员规则和包名匹配修正（7128a7b、75894fa）。
- **复现：**
  1. `TS_RECALL_REPO=<repo> TS_RECALL_OUT=g.jsonl TS_RECALL_SAMPLE='.ts|.tsx:200' [TS_RECALL_KINDS=Method,Field] cargo test -p astrolabe-mcp --lib refs_graph_ts::tests::dump_graph_answers -- --ignored --nocapture`
  2. `node lsrefs.js <repo> <tsconfig|js> g.jsonl ls.jsonl`
  3. `python3 cmpx.py g.jsonl ls.jsonl label [排除符号]`

  脚本在 `/workspace/audit/arch2026/ts-recall/`。

**顶层符号（Function / Class / Interface / Type，以及抽样里少量 Method）**

| 语料 / 用例 | 召回 第6批 → 7a → 7b | 精度 第6批 → 7a → 7b | 完全一致 第6批 → 7b |
|---|---|---|---|
| aiproxy web TS，第 6 批 40 例 | 0.992 → 1.000 → **1.000** | 0.998 → 0.998 → **0.998** | 38 → 39 |
| aiproxy web TS，抽样 200 | 0.984 → 1.000 → **1.000** | 0.998 → 0.998 → **0.998** | 188 → 197 |
| trpc monorepo TS，抽样 200 | **0.503** → 0.999 → **0.999** | 0.987 → 0.992 → **0.993** | 142 → 195 |
| express JS，第 6 批 30 例（不含 createApplication） | 1.000 → 1.000 → **1.000** | 0.631 → 0.631 → **0.883** | 17 → 24 |
| express JS，抽样 59（不含 createApplication） | 1.000 → 1.000 → **1.000** | 0.527 → 0.527 → **0.762** | 30 → 39 |

- **trpc 按种类（7b）：**

  | 种类 | 例数 | 召回 | 精度 | 引用数 |
  |---|---|---|---|---|
  | Class | 5 | 1.000 | 1.000 | 480 |
  | Function | 74 | 0.998 | 0.980 | 543 |
  | Type | 81 | 1.000 | 0.998 | 945 |
  | Interface | 34 | 0.996 | 1.000 | 236 |

  第 6 批在 monorepo 上只有一半召回，因为包之间都经 `@trpc/server`、`unstable-core-do-not-import` 这类 barrel 导入。
  - **7b 漏掉的 2 处：** 都在 JSDoc 的 `{@link X}` 里。我们排除注释，LanguageService 算这两处。
  - **7b 多报的 16 处：**
    - 不同接口里同名的类型参数 `TValue`。
    - 对象字面量键 `httpBatchStreamLink: httpBatchStreamLink(...)`。
    - 从调用返回值解构出的同名变量 `const { HydrateClient } = create…()`。
    - DOM 的同名方法 `signal.removeEventListener`。
- **索引作答耗时：** 中位数 18–30 ms，最长约 305 ms。

**成员（trpc Method/Field 120 例）**

| 种类 | 例数 | 召回 第6批 → 7b | 精度 第6批 → 7b |
|---|---|---|---|
| Method | 23 | 0.718 → 0.718 | 0.598 → 0.587 |
| Field | 97 | 0.832 → 0.884 | 0.324 → 0.313 |

成员经 `obj.m` 访问，名字层面分不出接收者类型，索引答案不能用，继续交给语言服务器。

**JS 的成员规则（7b）**
- **顶层自由函数、类、类型：** `x.name` 只有在 `x` 持有定义模块时才算，比如 `import * as ns`、`const m = require()`，或者 `export * as ns` 的绑定。
- **属性赋值式声明：** 例如 `app.handle = function handle(`、`exports.load = function(`。
  - 引用是成员访问。声明文件内只认同一对象或 `this`。
  - 声明文件内的裸名不算，包括函数表达式自身的名字和同名局部变量；同一行重复计数的问题也随之消失。
- **剩下的"多报"逐条看过，都不是误报：**
  - 5 处是 `this.enable(...)` 这类经 `this` 调用同一对象方法的地方。LanguageService 不把普通对象上的 `this` 连起来。
  - 2 处是 `lib/express.js` 里的 `app.handle` 和 `app.init`。`app` 混入了 application 原型，是真实引用。
  - 在 59 例集合里，另有 24 处出自 `res.status = function` 这类用例。LanguageService 对这些用例连声明都没返回（基线锚定失败），我抽查了一部分，都是真实引用。
  - 按人工判定，express 去掉 `createApplication` 后，剩下的多报里没有发现真误报。
- **`createApplication`：** 7b 给出 850 处。多出的是经 `require('..')`、`require('../../')` 再到根 `index.js`（`module.exports = require('./lib/express')`）绑定的本地名 `express`，绝大多数是 `express()` 调用。LanguageService 的 JS 推断不跟这条 CJS 链，所以这一例单列，不计入表里。

**结论（routing 未切换）**
- **TS/TSX 顶层符号**（Function、Class、Interface、Type，全仓唯一定义）可以切到索引优先。两个语料（单包应用、pnpm monorepo）上召回都 ≥0.999，精度 ≥0.993，与 Go 切换时的水平相当。前提是保留"多处定义就交给语言服务器"这条规则。
- **TS 的 Method/Field** 继续由语言服务器优先作答。
- **JS 顶层符号** 召回 1.000；按 LanguageService 口径精度是 0.76–0.88，按人工判定基本没有误报。但 express 是唯一的 JS 语料，而且大量引用依赖 CJS 链和 `this`，基线本身不完整。建议先在第二个 JS 语料（ESM 为主）上测过再切。
- **Vue** 没测，需要 vue 语言服务作基线。
