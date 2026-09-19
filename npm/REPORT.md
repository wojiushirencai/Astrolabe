# Astrolabe npm 分发实测报告

日期：2026-09-11。主机：macOS darwin-arm64，Node v24.11.1，npm 11.19.1。只改动了 `npm/`。

## 1. 包结构

`npm/` 是私有 workspace，**根 `package.json` 不发布**。真正上 registry 的是 `packages/` 里的七个包。

```
npm/
  package.json                     # private workspace
  scripts/fill-binaries.js         # CI / 本地二进制 → 平台包目录
  scripts/pack-all.js              # npm pack → dist/*.tgz
  scripts/publish.js               # 先平台包、后主包；可重跑
  scripts/verify-install.js        # MCP tools/list + 退出码 + SIGTERM
  packages/astrolabe/              # 主包 name: astrolabe，零 runtime 依赖
    bin/astrolabe.js               # shebang shim
    lib/{platforms,resolve,download,run}.js
  packages/darwin-arm64/           # @astrolabe/darwin-arm64
  packages/darwin-x64/
  packages/linux-x64-gnu/          # libc: glibc
  packages/linux-x64-musl/         # libc: musl
  packages/linux-arm64-gnu/
  packages/win32-x64-msvc/         # astrolabe.exe
```

主包 `optionalDependencies` 六个平台包，版本号与主包锁定为同一 `0.1.0`。npm 按 `os` / `cpu` / `libc` 只装匹配当前机器的那一个。

平台包 `bin` 名字是 `astrolabe-<platform>`，避免和主包的 `astrolabe` 抢 `node_modules/.bin`。shim 不走这个 bin 名，而是 `require.resolve('@astrolabe/<platform>/package.json')` 再拼二进制路径。

### GitHub Releases 契约（给并行流水线）

tag：`v${version}`。归档根目录放 `astrolabe` 或 `astrolabe.exe`，另附 GNU 格式 `SHA256SUMS`。

| 平台包 | rustc target | 归档 |
|---|---|---|
| `@astrolabe/darwin-arm64` | `aarch64-apple-darwin` | `astrolabe-aarch64-apple-darwin.tar.gz` |
| `@astrolabe/darwin-x64` | `x86_64-apple-darwin` | `astrolabe-x86_64-apple-darwin.tar.gz` |
| `@astrolabe/linux-x64-gnu` | `x86_64-unknown-linux-gnu` | `astrolabe-x86_64-unknown-linux-gnu.tar.gz` |
| `@astrolabe/linux-x64-musl` | `x86_64-unknown-linux-musl` | `astrolabe-x86_64-unknown-linux-musl.tar.gz` |
| `@astrolabe/linux-arm64-gnu` | `aarch64-unknown-linux-gnu` | `astrolabe-aarch64-unknown-linux-gnu.tar.gz` |
| `@astrolabe/win32-x64-msvc` | `x86_64-pc-windows-msvc` | `astrolabe-x86_64-pc-windows-msvc.zip` |

## 2. Shim 定位与回退

顺序：

1. `ASTROLABE_BINARY_PATH`（测试 / 开发覆盖）
2. `require.resolve('@astrolabe/<当前平台>')` 得到 optionalDependency 里的二进制，必要时 `chmod +x`
3. Yarn PnP 的 `.zip/` 虚路径会先拷到真实缓存再执行
4. 以上都失败（`--omit=optional`、`--ignore-scripts`、npm < 11 lockfile 丢 optionalDependencies、跨架构拷贝 `node_modules`）→ 从 GitHub Releases 拉 `SHA256SUMS` + 归档，**校验 SHA256 通过才解压**，写入 `~/.astrolabe/bin/<version>/<platform>/`（可用 `ASTROLABE_CACHE_DIR` 改），后续调用命中缓存。下载用目录锁，避免并发打穿。

stdio：`spawn(..., { stdio: 'inherit' })`。JSON-RPC 走 stdin/stdout，日志走 stderr，shim 不读、不缓冲、不改写。

退出码：子进程 `exit` 的 `code` 原样 `process.exit`。

信号：不用 `spawnSync`。`spawnSync` 会卡住事件循环，MCP 客户端对 shim PID 发 SIGTERM 时转不给 Rust。shim 把 SIGINT/SIGTERM/SIGHUP 转给子进程；子进程因信号退出时先摘掉自己的 handler 再 `process.kill(process.pid, signal)`，避免 handler 把信号吞掉导致挂死。

## 3. 可执行位怎么保证

没有 `postinstall`，pnpm 10+ 拦截依赖 lifecycle 也不受影响。

`npm pack` 对普通文件会归一到 `0644`，但对 **`package.json` 的 `bin` 字段指向的文件会打成 `0755`**。实测 tarball：

```
package/bin/astrolabe.js          -rwxr-xr-x     （主包 bin）
package/astrolabe                 -rwxr-xr-x     （平台包 bin → astrolabe-darwin-arm64）
```

安装后 npm 再按 `bin` 链一次，`.bin/astrolabe` 是指向 shim 的可执行 symlink。shim 在 spawn 前仍会补 `chmod`，覆盖手动解压 / PnP 拷出来的 0644。

## 4. 发布顺序与重试

`node scripts/publish.js`：

1. 核对七个 `package.json` 版本一致，且主包 `optionalDependencies` 指向同一版本
2. 六个平台包二进制都必须存在且 >100KB，否则拒绝发主包（`--dry-run` 同样检查）
3. **按平台包 → 主包的顺序 `npm publish`**
4. `npm view name@version` 已存在，或 409 / EPUBLISHCONFLICT → skip（可重跑）
5. 429 / 5xx / 网络错误最多 4 次指数退避
6. 任一平台包最终不在 registry 上，主包不发

本地矩阵不齐时的实测：

```
[publish] missing .../packages/darwin-x64/astrolabe; run fill-binaries.js --artifacts-dir … --require-all
publish-dry-run EXIT:1
```

CI 正确用法：`node scripts/fill-binaries.js --artifacts-dir <交叉编译产物> --require-all && node scripts/publish.js`

## 5. 本地实测（必须，已跑通）

填充：把 `target/release/astrolabe`（12 249 184 bytes）拷进 `packages/darwin-arm64/`，mode `755`。

### 5.1 npm pack 内容与权限

```
--- astrolabe-0.1.0.tgz ---
-rwxr-xr-x  0 0      0         334 Oct 26  1985 package/bin/astrolabe.js
-rw-r--r--  0 0      0        7107 Oct 26  1985 package/lib/download.js
-rw-r--r--  0 0      0        4024 Oct 26  1985 package/lib/platforms.js
-rw-r--r--  0 0      0        3313 Oct 26  1985 package/lib/resolve.js
-rw-r--r--  0 0      0        1674 Oct 26  1985 package/lib/run.js
-rw-r--r--  0 0      0        1016 Oct 26  1985 package/package.json
-rw-r--r--  0 0      0         733 Oct 26  1985 package/README.md
--- astrolabe-darwin-arm64-0.1.0.tgz ---
-rwxr-xr-x  0 0      0    12249184 Oct 26  1985 package/astrolabe
-rw-r--r--  0 0      0         621 Oct 26  1985 package/package.json
-rw-r--r--  0 0      0         348 Oct 26  1985 package/README.md
```

主包 tarball **不含** `packages/`、不含原生二进制。

### 5.2 临时目录安装 + npx

```
TMP=/tmp/astrolabe-npm-install-7UsE
npm install --omit=optional dist/astrolabe-darwin-arm64-0.1.0.tgz dist/astrolabe-0.1.0.tgz
# added 2 packages

lrwxr-xr-x  node_modules/.bin/astrolabe -> ../astrolabe/bin/astrolabe.js
-rwxr-xr-x  node_modules/astrolabe/bin/astrolabe.js
-rwxr-xr-x  node_modules/@astrolabe/darwin-arm64/astrolabe   12249184 bytes
npx which: /private/tmp/astrolabe-npm-install-7UsE/node_modules/.bin/astrolabe
```

`--omit=optional` 是为了不向 registry 拉尚未发布的平台包；平台包以 tarball 直装，模拟「optionalDependencies 装上了」的用户环境。

```
$ npx --no-install astrolabe /this/does/not/exist
Error: not a directory: /this/does/not/exist
npx missing-dir EXIT:1
```

shim 把 Rust 的 stderr 和退出码 1 原样透出。

### 5.3 MCP 握手：`tools/list` 返回 8 个工具

对已安装的 `.bin/astrolabe` 走真实 stdio（shim `stdio: inherit` → Rust）：无 `initialize` 直接发 `{"jsonrpc":"2.0","id":1,"method":"tools/list"}`。

```
tools/list count=8 ttlMs=86400000
tools: find_symbol, get_dependents, get_hotspots, get_languages, get_repo_skeleton, resolve_context, search_code, trace_calls
missing-dir exit code=1 signal=null
missing-dir stderr: Error: not a directory: .../no-such-dir
SIGTERM to shim -> code=null signal=SIGTERM
verify-install: ok
```

8 个工具、`ttlMs=86400000`、缺目录退出码 1、对 shim 发 SIGTERM 后父进程看到 `signal=SIGTERM`。

### 5.4 自愈回退（无平台包 + 假 Releases HTTP）

只装主包 tarball，`node_modules/@astrolabe` 不存在。用本机二进制打 `astrolabe-aarch64-apple-darwin.tar.gz`，配 `SHA256SUMS`，`python3 -m http.server` 冒充 GitHub Releases。

```
GET /SHA256SUMS HTTP/1.1" 200
GET /astrolabe-aarch64-apple-darwin.tar.gz HTTP/1.1" 200
Error: not a directory: /this/does/not/exist
fallback missing-dir EXIT:1
cache: ~/. 测试目录 /0.1.0/darwin-arm64/astrolabe  12249184 bytes  -rwxr-xr-x
```

第二次调用走缓存，再次 `tools/list` = 8，SIGTERM 透传。下载过程中 shim 不读 stdin，后续 `inherit` 仍把 MCP 管道交给二进制。

### 5.5 SHA256 失败则拒绝执行

把 `SHA256SUMS` 改成全 0，调用 `downloadFromGitHub`：

```
SHA256 mismatch for astrolabe-aarch64-apple-darwin.tar.gz: expected 0000…0000, got 5f327648e611beb88e5c229f3f1a38216de6950fd9cd8af807e555729f517202
cached binary present: false
sha256-mismatch fail-closed: ok
```

哈希不对不写缓存、不 spawn。

## 6. 发布注意事项

registry 上已有 2014 年的 Protractor 辅助包占用未加 scope 的 [`astrolabe`](https://www.npmjs.com/package/astrolabe)。产品命令仍是 `npx -y astrolabe`，首次 `npm publish` 可能被拒，需要 npm 回收名或改名。`@astrolabe/*` 需要先建 npm org。

许可字段已写 `MIT OR Apache-2.0`，与 Cargo workspace 一致。LICENSE 正文由并行任务放在仓库根；主包 `files` 未内嵌，避免和 LICENSE 任务双写。

## 7. 复现命令

```bash
cd npm
node scripts/fill-binaries.js --binary ../target/release/astrolabe
node scripts/pack-all.js
# 在干净目录：npm install --omit=optional dist/*.tgz
# node scripts/verify-install.js --bin <install>/node_modules/.bin/astrolabe --root <repo>
```
