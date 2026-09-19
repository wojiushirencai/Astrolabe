# 发布 Astrolabe

本文说明如何把 `astrolabe` 二进制发到 GitHub Releases。流水线文件是 [workflows/release.yml](workflows/release.yml)，在推送 `v*` tag 时构建六个目标三元组并上传归档、SHA256 校验和与双许可证。

下游 npm 包装（若接入）会下载这些归档并用校验和做完整性校验。**平台 npm 包必须先于主包发布**，见下文「发布顺序」。

## 产物

每个 tag `vX.Y.Z` 会生成一个 GitHub Release，资源命名统一，含版本号和 Rust target 三元组：

| 文件 | 说明 |
| --- | --- |
| `astrolabe-X.Y.Z-aarch64-apple-darwin.tar.gz` | macOS Apple Silicon |
| `astrolabe-X.Y.Z-x86_64-apple-darwin.tar.gz` | macOS Intel |
| `astrolabe-X.Y.Z-x86_64-unknown-linux-gnu.tar.gz` | Linux x64（glibc，随 ubuntu-24.04 链接） |
| `astrolabe-X.Y.Z-x86_64-unknown-linux-musl.tar.gz` | Linux x64 静态链接 |
| `astrolabe-X.Y.Z-aarch64-unknown-linux-gnu.tar.gz` | Linux ARM64 |
| `astrolabe-X.Y.Z-x86_64-pc-windows-msvc.zip` | Windows x64 |
| `*.tar.gz.sha256` / `*.zip.sha256` | 单个归档的 SHA256（GNU `sha256sum` 格式：`hash␠␠filename`） |
| `SHA256SUMS` | 六个归档的合集，可用 `sha256sum -c SHA256SUMS` 校验 |

归档根目录内容：

- Unix：`astrolabe`、`LICENSE-MIT`、`LICENSE-APACHE`
- Windows：`astrolabe.exe`、`LICENSE-MIT`、`LICENSE-APACHE`

GNU Linux 归档链接的是构建 runner 上的 glibc（当前为 Ubuntu 24.04 / glibc 2.39）。更老的发行版请用 musl 归档。

## 发布前检查清单

打 tag 之前在**即将发布的那个 commit**上确认：

1. **测试**
   - `cargo test --workspace --locked` 通过。
   - 若 CI 工作流已存在，该 commit 的 CI 是绿的。
2. **验收**
   - 用 `cargo build --release --bin astrolabe` 在本机跑一遍核心路径（索引、查询、MCP stdio）。
   - 确认没有把调试日志、本机路径或密钥写进默认输出。
3. **版本号一致**
   - `Cargo.toml` 里 `[workspace.package] version` 等于即将打的 tag（去掉 `v` 前缀）。
   - 流水线会再检查一次：tag `v0.2.0` 对不上 `0.1.0` 会直接失败。
   - `crates/astrolabe-mcp` 与 `crates/astrolabe-core` 使用 `version.workspace = true`，不要在成员 crate 里另写版本。
4. **CHANGELOG**
   - 仓库根目录 `CHANGELOG.md` 应有对应小节，标题可为 `## [0.1.0]`、`## 0.1.0` 或带 `v` 前缀。
   - 没有 CHANGELOG、或没有该版本小节时，Release 正文会退回 GitHub 根据 commit 自动生成的 notes。有小节时，该小节会写在自动 notes 前面。
5. **许可证**
   - 根目录必须有 `LICENSE-MIT` 和 `LICENSE-APACHE`（归档会打进去）。
6. **锁文件**
   - `Cargo.lock` 已提交，且与 `Cargo.toml` 同步。发布构建使用 `--locked`。
7. **npm（若本版本要发 npm）**
   - 平台包的下载 URL、文件名、SHA256 字段能对上上面的命名。
   - 先不要 `npm publish` 主包。见「发布顺序」。

## 打 tag 的步骤

在已推送到 `origin` 的发布 commit 上操作（把 `0.1.0` 换成实际版本）：

```bash
# 确认工作区干净、版本号已改完并提交
git status
git log -1

# 推荐 annotated tag：消息会进 git 历史，也方便日后核对
git tag -a v0.1.0 -m "Astrolabe 0.1.0"

# 只推这个 tag，不要 git push --tags（以免带上本地实验 tag）
git push origin v0.1.0
```

推送后打开 Actions 里的 **release** 工作流。六个 `build` 矩阵全部成功后，`github-release` job 会建 Release 并上传文件。

预发布版本用带连字符的 tag，例如 `v0.1.0-rc.1`。流水线会把含 `-` 的 tag 标成 GitHub prerelease，不会标成 Latest。

不要把同一个版本号改来改去后重复打成同一个 tag。需要重做时走下面的「失败如何重试」，不要 `git tag -f` 除非你明确要改写已公开的 tag。

## 发布顺序（接 npm 时的坑）

GitHub Release 是 npm 平台包的字节来源。以后若用 `optionalDependencies` 按平台分发：

1. **先等 GitHub Release 完整上线**  
   六个归档 + 六个 `.sha256` + `SHA256SUMS` 都在 Release 页面上，抽查一个 `sha256sum -c`。
2. **再发布六个（或实际支持的）平台 npm 包**  
   平台包的 `postinstall` / 下载脚本按 target 拉对应归档，并用 Release 上的 SHA256 校验。平台包之间不要互相依赖。
3. **最后发布主包**  
   主包通过 `optionalDependencies` 引用平台包的**精确版本**。npm 安装主包时会去取平台包；若主包已经在 registry 里、平台包还没有，用户 `npm install astrolabe` 会失败。
4. **不要反过来，也不要并行碰主包**  
   主包一旦以某版本出现在 registry，安装器就会解析该版本的 optional deps。缺一个平台包就是一次失败的安装。

建议把「GitHub Release URL 已 200 + SHA256 对得上」做成 npm 发布脚本的前置断言。

## 失败如何重试

分清两件事：**GitHub Release 可以删了再建**；**npm 版本号一旦 publish 就不可变**。

### 工作流还在跑 / 构建失败

1. 打开该次 workflow run，看是哪个 target 红了。  
   `fail-fast: false`，所以其他平台仍会跑完，便于一次看清所有失败。
2. 修代码或修工作流后，**不要重新打相同 tag**。把修复 commit 推上去，删掉远程 tag 再打到新 commit（见下），或对仍停留在错误 commit 上的 tag 选择「Re-run failed jobs / Re-run all jobs」。
3. 只重跑失败 job 时：若 `build` 已成功、只是 `github-release` 失败，优先 **Re-run failed jobs**。构建产物作为 artifact 还在（保留 7 天），不必再烧一遍 macOS 分钟。
4. 六个 `build` 必须全绿，`github-release` 才会跑。缺一个平台就不会发半套 Release——这是有意的，避免 npm 校验和指向不存在的文件。

### GitHub Release 已创建但内容不对

1. 在 GitHub 网页删除该 Release（**只删 Release，先不要删 tag**）。
2. 在 Actions 对同一次 tag 的 run 选 **Re-run all jobs**。  
   `softprops/action-gh-release` 开了 `overwrite_files: true`：若 Release 还在、只是个别 asset 坏了，也可以直接重跑 `github-release` 覆盖同名文件。
3. 只有在 tag 指错 commit 时才改 tag：

   ```bash
   git tag -d v0.1.0
   git push origin :refs/tags/v0.1.0
   git tag -a v0.1.0 -m "Astrolabe 0.1.0"
   git push origin v0.1.0
   ```

   改写已公开 tag 会让已经按旧 SHA 下载的人校验失败。能 bump 补丁版就不要改写 tag。

### npm 已经 publish 失败或发错

- `npm publish` 成功后，**同一版本不能再 publish**。unpublish 有 72 小时限制，且对已经安装过的用户不友好。
- GitHub Release 可以删重建；npm 不能。所以顺序必须是 Release → 平台包 → 主包。
- 若平台包已发布、二进制后来重做了：必须 **bump 版本**（例如 `0.1.1`），同时重发 GitHub Release 和所有 npm 包。不要试图原地替换平台包里的 URL 指向另一份字节却保留 `0.1.0`。
- 若只有主包发了、平台包没发：主包无法覆盖。发齐平台包后，失败的安装会在下一次 `npm install` 时好。不要把主包 yank 掉再发同一个版本。

## macOS 签名与 Gatekeeper

流水线**不对** macOS 二进制做 Developer ID 签名，也**不做**公证（notarization）。这需要 Apple Developer Program（年费）和证书保管，当前发布范围不包括。

用户第一次从浏览器下载 `tar.gz` 再运行时，常见现象：

- 「无法打开，因为无法验证开发者」
- 「已损坏，无法打开」
- 双击没有反应，或被隔离（quarantine）

绕过（任选，发给用户时写清楚风险：只对自己信任的 Release 做）：

```bash
# 1. 解压后去掉隔离属性（最常见）
xattr -d com.apple.quarantine ./astrolabe
./astrolabe --help

# 2. 或在「系统设置 → 隐私与安全性」里点「仍要打开」
# 3. 或右键 Dock / Finder 图标 → 打开，确认一次
```

从 `curl` / `gh release download` 拿到的文件有时没有 quarantine，可以直接跑。Homebrew 或以后的 npm 包装若自己负责放置二进制，用户通常碰不到这层对话框。

以后若要去掉这层摩擦：用 Developer ID Application 签名，再 `notarytool submit` + `stapler staple`。那是独立工作，不要在未配置证书时往本工作流里加签名步骤。

## 构建矩阵与 C 工具链

`astrolabe-core` 依赖 `tree-sitter` 0.27 以及 python/go/java/rust/typescript/javascript 六套语法 crate。它们的 `build.rs` 通过 `cc` 编译 `parser.c`（部分语法还有 scanner）。这不是纯 Rust 交叉编译：每个 target 都要有**能产出该 target 目标码的 C 编译器**。

因此矩阵用**原生 runner**，只有 musl 交叉：

| target | runner | C 工具链 |
| --- | --- | --- |
| `aarch64-apple-darwin` | `macos-15`（Apple Silicon） | 系统 clang |
| `x86_64-apple-darwin` | `macos-15-intel` | 系统 clang（原生 Intel，不用 `-arch` 交叉） |
| `x86_64-unknown-linux-gnu` | `ubuntu-24.04` | 系统 gcc/cc |
| `x86_64-unknown-linux-musl` | `ubuntu-24.04` + `taiki-e/setup-cross-toolchain-action` | musl 1.2 gcc/g++，并导出 `CC_*` / `CXX_*` / `LINKER` |
| `aarch64-unknown-linux-gnu` | `ubuntu-24.04-arm` | 系统 gcc（原生 ARM，不用 `aarch64-linux-gnu-gcc`） |
| `x86_64-pc-windows-msvc` | `windows-latest` | MSVC `cl.exe`（语法 crate 在 msvc 上会加 `-utf-8`） |

不用 Docker `cross` 的原因：它只覆盖 Linux 容器，产不出 macOS / MSVC 产物；GitHub 已提供 `ubuntu-24.04-arm`；`cc` 编 tree-sitter 时原生编译器最不容易踩 sysroot。musl 若只用 `apt install musl-tools`，`cc` 默认找的是 `x86_64-unknown-linux-musl-gcc`，Ubuntu 提供的却是 `musl-gcc`，必须手动设 `CC_x86_64_unknown_linux_musl`。`setup-cross-toolchain-action` 会装好带 sysroot 的交叉 gcc 并写好这些变量。

`macos-15-intel` 是 GitHub 在停掉 `macos-13` 之后提供的标准 Intel 标签，计划支撑到 2027-08。之后若要继续发 Intel 归档，只能在 Apple Silicon runner 上用 clang `-arch x86_64` 交叉（`cc` crate 对 `x86_64-apple-darwin` 会自动加该参数）。

## 校验和约定

- 算法：SHA256。
- 在 `ubuntu-24.04` 的 `github-release` job 里用 GNU `sha256sum` 统一生成，避免 Windows `Get-FileHash` / macOS `shasum` 格式不一致。
- 单文件：`astrolabe-X.Y.Z-<target>.tar.gz.sha256`，内容一行：`<hex>  astrolabe-X.Y.Z-<target>.tar.gz`（两个空格）。
- 合集：`SHA256SUMS`，同样六行。
- npm 侧读取时用第一个空白字段当 hex 即可；不要假设文件里只有 hash、没有文件名。

## 本机无法触发 Actions 时

改工作流后至少做静态检查：

```bash
python3 -c "import yaml,pathlib,sys; yaml.safe_load(pathlib.Path('.github/workflows/release.yml').read_text())"
# 若已安装：
yamllint .github/workflows/release.yml
actionlint .github/workflows/release.yml
```

真正的交叉编译和 tree-sitter 的 `cc` 调用只有推 tag 后才验证。第一次发布请预留时间看 musl job 的「Show compilers」和 `cargo build` 日志。
