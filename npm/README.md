# Astrolabe npm 分发

给没有 Rust 工具链的用户用 `npx` 跑 Astrolabe MCP server。主包是零依赖 Node shim，六个平台包通过 `optionalDependencies` 按 `os` / `cpu` / `libc` 各装一个二进制。

```bash
npx -y astrolabe /path/to/repo
```

本目录是独立的 npm workspace。**不要**把 `packages/` 打进主包 tarball。

## 布局

```
npm/
  package.json                  私有 workspace 根，不发布
  scripts/fill-binaries.js      把 CI / 本地二进制填进平台包
  scripts/pack-all.js           npm pack → dist/*.tgz
  scripts/publish.js            先发六个平台包，再发主包；可重跑
  scripts/verify-install.js     MCP tools/list + 退出码实测
  packages/astrolabe/           发布名 astrolabe（shim）
  packages/<platform>/          发布名 @astrolabe/<platform>
```

## GitHub Releases 契约

给 Releases 流水线对齐用。tag 为 `v${version}`，例如 `v0.1.0`。

| 平台包 | rustc target | 归档 |
|---|---|---|
| `@astrolabe/darwin-arm64` | `aarch64-apple-darwin` | `astrolabe-aarch64-apple-darwin.tar.gz` |
| `@astrolabe/darwin-x64` | `x86_64-apple-darwin` | `astrolabe-x86_64-apple-darwin.tar.gz` |
| `@astrolabe/linux-x64-gnu` | `x86_64-unknown-linux-gnu` | `astrolabe-x86_64-unknown-linux-gnu.tar.gz` |
| `@astrolabe/linux-x64-musl` | `x86_64-unknown-linux-musl` | `astrolabe-x86_64-unknown-linux-musl.tar.gz` |
| `@astrolabe/linux-arm64-gnu` | `aarch64-unknown-linux-gnu` | `astrolabe-aarch64-unknown-linux-gnu.tar.gz` |
| `@astrolabe/win32-x64-msvc` | `x86_64-pc-windows-msvc` | `astrolabe-x86_64-pc-windows-msvc.zip` |

另外必须有 `SHA256SUMS`（GNU coreutils：`<hex>  <filename>`）。归档根目录放 `astrolabe` 或 `astrolabe.exe`。

自愈回退下载地址：

```
https://gitee.com/adam_1986/astrolabe/releases/download/v${version}/<archive>
https://gitee.com/adam_1986/astrolabe/releases/download/v${version}/SHA256SUMS
```

可用下列变量覆盖：

| 变量 | 作用 |
|---|---|
| `ASTROLABE_RELEASES_BASE_URL` | 整段替换下载基址，优先级最高 |
| `ASTROLABE_RELEASE_HOST` | 只换主机，默认 `https://gitee.com` |
| `ASTROLABE_RELEASE_REPO` | 只换仓库，默认 `adam_1986/astrolabe` |
| `ASTROLABE_RELEASE_TOKEN` | 私有 release 的访问凭据 |
| `ASTROLABE_CACHE_DIR` | 二进制缓存目录 |
| `ASTROLABE_BINARY_PATH` | 直接指定二进制，跳过下载 |

凭据只发给签发它的主机：`GITHUB_TOKEN` 仅在下载地址确实指向 github.com 时附带。
CI 环境普遍设置了该变量，无条件附带会把 GitHub 凭据交给无关主机。

## 填充、打包、发布

```bash
# 本地：把当前机器的 release 二进制填进对应平台包
node scripts/fill-binaries.js --binary ../target/release/astrolabe

# CI：把交叉编译产物填进全部六个包
node scripts/fill-binaries.js --artifacts-dir ./dist --require-all

# 打包（缺二进制的平台会跳过；CI 请加 --require-all）
node scripts/pack-all.js

# 发布：平台包全部成功（或已在 registry）之后才发主包
node scripts/publish.js [--dry-run] [--otp …] [--tag latest]
```

发布非原子。中途失败直接重跑：已在 registry 的版本会 skip，不会覆盖。主包在任一平台包缺失时拒绝发布。

## 可执行位

`npm pack` 会把普通文件模式归一到 `0644`。平台包用 `bin` 字段指向二进制（名字是 `astrolabe-<platform>`，避免和主包的 `astrolabe` 抢 PATH），安装器据此 chmod `0755`。没有 `postinstall`，pnpm 10+ 默认拦截依赖 lifecycle 也不受影响。shim 在 spawn 前仍会补一次 chmod，覆盖手动解压或 Yarn PnP 虚拟路径的情况。

## 已知的包名占用

registry 上已有一个 2014 年的 Protractor 辅助包占用了未加 scope 的 [`astrolabe`](https://www.npmjs.com/package/astrolabe)。首次 `npm publish` 可能被拒，需要 npm 支持回收或改名。产品命令按 README 仍是 `npx -y astrolabe`。
