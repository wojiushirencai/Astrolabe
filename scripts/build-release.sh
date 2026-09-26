#!/usr/bin/env bash
# ==============================================================================
# build-release.sh - 跨平台发布编译脚本（macOS + Windows 双平台，带版本号）
#
# 规则约束：
# 1. 每次发布必须同时构建 macOS 与 Windows 两个平台版本。
# 2. 产物必须包含版本号标识（例如 astrolabe-v0.1.0-*），同时保留规范 target 三元组与无版本软链接/别名。
# 3. 归档包内必须包含二进制及 LICENSE。
# 4. 自动生成包含全部归档的 SHA256SUMS 校验文件。
# ==============================================================================
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
cd "${REPO_ROOT}"

# 1. 提取或校验版本号（限定 [workspace.package] 段，避免误匹配依赖行）
VERSION="${1:-}"
if [[ -z "$VERSION" ]]; then
    VERSION=$(sed -n '/^\[workspace\.package\]/,/^\[/p' Cargo.toml | grep -E '^version\s*=' | head -n 1 | sed -E 's/.*"([^"]+)".*/\1/')
fi
VERSION="${VERSION#v}" # 确保纯版本号数字如 0.1.0

if [[ -z "$VERSION" ]]; then
    echo "错误：未能从 Cargo.toml 提取到版本号，请显式传入版本号，例如：./scripts/build-release.sh 0.1.0" >&2
    exit 1
fi

echo "=================================================="
echo "准备编译发布 Astrolabe 版本: v${VERSION}"
echo "发布平台: macOS (arm64, x86_64, universal) + Windows (x64)"
echo "=================================================="

# 2. 检查依赖工具
command -v cargo >/dev/null 2>&1 || { echo "错误: 未找到 cargo" >&2; exit 1; }
command -v cargo-xwin >/dev/null 2>&1 || { echo "错误: 未找到 cargo-xwin (用于 Windows 交叉编译)" >&2; exit 1; }
command -v lipo >/dev/null 2>&1 || { echo "错误: 未找到 lipo" >&2; exit 1; }
command -v zip >/dev/null 2>&1 || { echo "错误: 未找到 zip (用于 Windows 归档)" >&2; exit 1; }

# SHA256 工具：优先 GNU sha256sum，回退 macOS 自带 shasum（两者输出同构 `<hex>  <file>`）
if command -v sha256sum >/dev/null 2>&1; then
    SHA256=(sha256sum)
elif command -v shasum >/dev/null 2>&1; then
    SHA256=(shasum -a 256)
else
    echo "错误: 未找到 sha256sum 或 shasum" >&2
    exit 1
fi

# macOS 交叉目标需已安装（rust-toolchain.toml 固定的工具链下）
if command -v rustup >/dev/null 2>&1; then
    for t in aarch64-apple-darwin x86_64-apple-darwin; do
        if ! rustup target list --installed 2>/dev/null | grep -qx "$t"; then
            echo "错误: rustup 未安装 target ${t}，请先执行 rustup target add ${t}" >&2
            exit 1
        fi
    done
fi

DIST_DIR="${REPO_ROOT}/dist/release-binaries"
STAGING_DIR="${REPO_ROOT}/target/release-staging"
rm -rf "${STAGING_DIR}"
mkdir -p "${DIST_DIR}" "${STAGING_DIR}"

if [[ ! -f LICENSE ]]; then
    echo "错误: 仓库根目录下必须存在 LICENSE" >&2
    exit 1
fi

# 3. 编译各平台目标（--locked：Cargo.lock 与 Cargo.toml 不同步时立即失败，保证可复现）
echo ""
echo ">>> [1/4] 编译 macOS Apple Silicon (aarch64-apple-darwin)..."
cargo build --locked --release --bin astrolabe --target aarch64-apple-darwin

echo ""
echo ">>> [2/4] 编译 macOS Intel (x86_64-apple-darwin)..."
cargo build --locked --release --bin astrolabe --target x86_64-apple-darwin

echo ""
echo ">>> [3/4] 合成 macOS 通用二进制 (Universal: arm64 + x86_64)..."
ARM64_BIN="target/aarch64-apple-darwin/release/astrolabe"
X86_BIN="target/x86_64-apple-darwin/release/astrolabe"
UNIVERSAL_BIN="${STAGING_DIR}/astrolabe-universal"
lipo -create -output "${UNIVERSAL_BIN}" "${ARM64_BIN}" "${X86_BIN}"

echo ""
echo ">>> [4/4] 编译 Windows x64 (x86_64-pc-windows-msvc)..."
cargo xwin build --locked --release --bin astrolabe --target x86_64-pc-windows-msvc
WIN_BIN="target/x86_64-pc-windows-msvc/release/astrolabe.exe"

# 4. 打包并输出带版本号与规范命名的产物
echo ""
echo ">>> 正在打包发布产物到 ${DIST_DIR}..."

# 收集本次生成的全部归档，最后统一生成 SHA256SUMS
ARCHIVES=()

package_unix() {
    local label="$1"
    local bin_src="$2"
    local target_triple="$3"

    local work_dir="${STAGING_DIR}/${label}"
    rm -rf "${work_dir}"
    mkdir -p "${work_dir}"
    cp "${bin_src}" "${work_dir}/astrolabe"
    chmod +x "${work_dir}/astrolabe"
    cp LICENSE "${work_dir}/"

    # 1. 复制带版本号及常规别名的独立二进制
    cp "${bin_src}" "${DIST_DIR}/astrolabe-v${VERSION}-${label}"
    cp "${bin_src}" "${DIST_DIR}/astrolabe-${label}"
    chmod +x "${DIST_DIR}/astrolabe-v${VERSION}-${label}" "${DIST_DIR}/astrolabe-${label}"

    # 2. 打包 tar.gz 归档（含版本号）
    tar -czf "${DIST_DIR}/astrolabe-v${VERSION}-${label}.tar.gz" -C "${work_dir}" astrolabe LICENSE
    # 建立兼容别名：
    #   - 无版本号标签名（astrolabe-macos-arm64.tar.gz）
    #   - 带版本号规范三元组名（astrolabe-0.1.0-aarch64-apple-darwin.tar.gz，与 CI release.yml 命名一致）
    #   - 无版本号规范三元组名（astrolabe-aarch64-apple-darwin.tar.gz，与 npm fill-binaries/download.js 消费契约一致）
    cp "${DIST_DIR}/astrolabe-v${VERSION}-${label}.tar.gz" "${DIST_DIR}/astrolabe-${label}.tar.gz"
    ARCHIVES+=("${DIST_DIR}/astrolabe-v${VERSION}-${label}.tar.gz" "${DIST_DIR}/astrolabe-${label}.tar.gz")
    if [[ -n "${target_triple}" ]]; then
        cp "${DIST_DIR}/astrolabe-v${VERSION}-${label}.tar.gz" "${DIST_DIR}/astrolabe-${VERSION}-${target_triple}.tar.gz"
        cp "${DIST_DIR}/astrolabe-v${VERSION}-${label}.tar.gz" "${DIST_DIR}/astrolabe-${target_triple}.tar.gz"
        ARCHIVES+=("${DIST_DIR}/astrolabe-${VERSION}-${target_triple}.tar.gz" "${DIST_DIR}/astrolabe-${target_triple}.tar.gz")
    fi
}

package_windows() {
    local label="$1"
    local bin_src="$2"
    local target_triple="$3"

    local work_dir="${STAGING_DIR}/${label}"
    rm -rf "${work_dir}"
    mkdir -p "${work_dir}"
    cp "${bin_src}" "${work_dir}/astrolabe.exe"
    cp LICENSE "${work_dir}/"

    # 1. 复制带版本号及常规别名的独立可执行文件
    cp "${bin_src}" "${DIST_DIR}/astrolabe-v${VERSION}-${label}.exe"
    cp "${bin_src}" "${DIST_DIR}/astrolabe-${label}.exe"

    # 2. 打包 zip 归档（含版本号）
    (
        cd "${work_dir}"
        zip -q -9 "${DIST_DIR}/astrolabe-v${VERSION}-${label}.zip" astrolabe.exe LICENSE
    )
    # 建立兼容别名（命名规则与 package_unix 相同）
    cp "${DIST_DIR}/astrolabe-v${VERSION}-${label}.zip" "${DIST_DIR}/astrolabe-${label}.zip"
    ARCHIVES+=("${DIST_DIR}/astrolabe-v${VERSION}-${label}.zip" "${DIST_DIR}/astrolabe-${label}.zip")
    if [[ -n "${target_triple}" ]]; then
        cp "${DIST_DIR}/astrolabe-v${VERSION}-${label}.zip" "${DIST_DIR}/astrolabe-${VERSION}-${target_triple}.zip"
        cp "${DIST_DIR}/astrolabe-v${VERSION}-${label}.zip" "${DIST_DIR}/astrolabe-${target_triple}.zip"
        ARCHIVES+=("${DIST_DIR}/astrolabe-${VERSION}-${target_triple}.zip" "${DIST_DIR}/astrolabe-${target_triple}.zip")
    fi
}

package_unix "macos-arm64" "${ARM64_BIN}" "aarch64-apple-darwin"
package_unix "macos-x86_64" "${X86_BIN}" "x86_64-apple-darwin"
package_unix "macos-universal" "${UNIVERSAL_BIN}" "universal-apple-darwin"
package_windows "windows-x64" "${WIN_BIN}" "x86_64-pc-windows-msvc"

# 5. 生成校验和（覆盖本次生成的全部归档，含各命名别名）
echo ""
echo ">>> 计算 SHA256SUMS..."
cd "${DIST_DIR}"
"${SHA256[@]}" "${ARCHIVES[@]##*/}" > SHA256SUMS

cd "${REPO_ROOT}"
rm -rf "${STAGING_DIR}"

# 6. 验证二进制
echo ""
echo ">>> 验证本地二进制版本号:"
"${DIST_DIR}/astrolabe-v${VERSION}-macos-arm64" --version

echo ""
echo "=================================================="
echo "发布构建完成！产物列表（${DIST_DIR}）:"
ls -lh "${DIST_DIR}"
echo "=================================================="
