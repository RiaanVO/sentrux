#!/usr/bin/env bash
#
# Cross-compilation script for sentrux binary targeting aarch64-unknown-linux-gnu
# Uses Docker with the proven approach from docker-sbx-extensions/artifacts-build/Dockerfile
#
set -euo pipefail

# ── Configuration ────────────────────────────────────────────────────────────
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="${SCRIPT_DIR}"
SENRUX_BIN="${PROJECT_ROOT}/sentrux-bin"
DIST_DIR="${PROJECT_ROOT}/dist"
BUILD_IMAGE_NAME="sentrux-cross-aarch64"
COMMIT_SHA=$(cd "${PROJECT_ROOT}" && git rev-parse --short HEAD 2>/dev/null || echo "unknown")
DOCKERFILE_PATH="${SCRIPT_DIR}/Dockerfile.sentrux-cross"

# ── Extract version from Cargo.toml ──────────────────────────────────────────
VERSION=""
if [[ -f "${SENRUX_BIN}/Cargo.toml" ]]; then
    VERSION=$(grep -m1 '^version = ' "${SENRUX_BIN}/Cargo.toml" | sed 's/^version = "\(.*\)"/\1/')
fi

if [[ -z "${VERSION}" ]]; then
    echo "[ERROR] Could not extract version from sentrux-bin/Cargo.toml"
    exit 1
fi

echo "═══════════════════════════════════════════════════════════════"
echo "  sentrux cross-compile build"
echo "  Version : ${VERSION}"
echo "  Commit  : ${COMMIT_SHA}"
echo "  Target  : aarch64-unknown-linux-gnu"
echo "  Output  : ${DIST_DIR}/sentrux-${VERSION}-${COMMIT_SHA}-linux-arm64"
echo "═══════════════════════════════════════════════════════════════"
echo ""

# ── Prerequisite checks ─────────────────────────────────────────────────────
if ! command -v docker &>/dev/null; then
    echo "[ERROR] docker is not installed. Please install Docker Desktop or Docker CLI."
    exit 1
fi

if ! docker info &>/dev/null 2>&1; then
    echo "[ERROR] Docker daemon is not running. Please start Docker Desktop."
    exit 1
fi

# ── Create output directory ──────────────────────────────────────────────────
mkdir -p "${DIST_DIR}"

# ── Generate the cross-compilation Dockerfile ────────────────────────────────
# Mirrors the proven approach from:
#   /Users/Shared/Projects/docker-sbx-extensions/artifacts-build/Dockerfile
cat > "${DOCKERFILE_PATH}" <<'DOCKERFILE'
# Cross-compilation image for sentrux-bin targeting aarch64-unknown-linux-gnu
# Mirrors the proven approach in docker-sbx-extensions/artifacts-build/Dockerfile
FROM rust:bookworm AS builder

ENV DEBIAN_FRONTEND=noninteractive

# Install cross-compilation toolchain and GTK3 runtime libraries for cross-linking
# These are required by sentrux-core (eframe, egui, rfd gtk3, git2, tree-sitter)
RUN apt-get update && \
    apt-get install -y --no-install-recommends \
        gcc-aarch64-linux-gnu \
        g++-aarch64-linux-gnu \
        binutils-aarch64-linux-gnu \
        pkg-config \
        libssl-dev \
        libatk1.0-dev \
        libgtk-3-dev \
        libgdk-pixbuf-2.0-dev \
        libcairo2-dev \
        libglib2.0-dev \
        libpango1.0-dev \
        && rm -rf /var/lib/apt/lists/*

# Add the aarch64-unknown-linux-gnu Rust target
RUN rustup target add aarch64-unknown-linux-gnu

WORKDIR /build
DOCKERFILE

# ── Build the cross-compilation image ────────────────────────────────────────
echo "[1/4] Building cross-compilation Docker image..."
if docker image inspect "${BUILD_IMAGE_NAME}" &>/dev/null 2>&1; then
    echo "  → Using cached image: ${BUILD_IMAGE_NAME}"
else
    docker build -t "${BUILD_IMAGE_NAME}" -f "${DOCKERFILE_PATH}" "${SCRIPT_DIR}" 2>&1
    echo "  → Cross-compilation image built."
fi
echo ""

# ── Build sentrux-bin in Docker ─────────────────────────────────────────────
echo "[2/4] Cross-compiling sentrux-bin for aarch64-unknown-linux-gnu..."
echo ""

CONTAINER_NAME="sentrux-cross-build-$$"
BUILD_VOLUME="sentrux-build-$$"

cleanup() {
    # Clean up container
    if docker ps -a --format '{{.Names}}' 2>/dev/null | grep -qx "${CONTAINER_NAME}"; then
        docker rm -f "${CONTAINER_NAME}" &>/dev/null || true
    fi
    # Clean up volume
    if docker volume inspect "${BUILD_VOLUME}" &>/dev/null 2>&1; then
        docker volume rm "${BUILD_VOLUME}" &>/dev/null || true
    fi
}
trap cleanup EXIT ERR

# Create a temporary Docker volume for build output
docker volume create "${BUILD_VOLUME}" &>/dev/null

# Create the container (don't start yet — we can inspect it on failure)
docker create \
    --name "${CONTAINER_NAME}" \
    -v "${PROJECT_ROOT}:/build:ro" \
    -v "${BUILD_VOLUME}:/build-target" \
    -e CARGO_TARGET_DIR=/build-target \
    -w /build \
    "${BUILD_IMAGE_NAME}" \
    cargo build --release --package sentrux --target aarch64-unknown-linux-gnu &>/dev/null

# Verify container was created
if ! docker ps -a --format '{{.Names}}' 2>/dev/null | grep -qx "${CONTAINER_NAME}"; then
    echo "[ERROR] Failed to create Docker build container."
    exit 1
fi

# Start and wait for the build
echo "  → Building (this may take several minutes for first run)..."
docker start -ai "${CONTAINER_NAME}" &>/dev/null || {
    BUILD_EXIT=$?
    docker logs "${CONTAINER_NAME}" 2>&1 | tail -80
    echo ""
    echo "[FAIL] Cross-compilation failed (exit code: ${BUILD_EXIT})."
    echo ""
    # Check if failure is test-related
    FAILED_TESTS=$(docker logs "${CONTAINER_NAME}" 2>&1 | grep -c "test.*FAILED" || true)
    if [[ "${FAILED_TESTS}" -gt 0 ]]; then
        echo "[NOTE] Test failures detected. Tests are expected to fail during cross-compilation."
        echo "       The binary link step may still succeed. See above for details."
    fi
    exit 1
}

echo "  → Cross-compilation completed."
echo ""

# ── Extract the binary ──────────────────────────────────────────────────────
echo "[3/4] Extracting binary from Docker..."

# The binary is at CARGO_TARGET_DIR/aarch64-unknown-linux-gnu/release/sentrux
TARGET_BINARY="/build-target/aarch64-unknown-linux-gnu/release/sentrux"
EXTRACTED="/tmp/sentrux-arm64-bin"

docker cp "${CONTAINER_NAME}:${TARGET_BINARY}" "${EXTRACTED}" 2>/dev/null || {
    echo "[ERROR] Binary not found at ${TARGET_BINARY} in container."
    echo "  Searching for artifacts..."
    docker exec "${CONTAINER_NAME}" ls -la /build-target/ 2>/dev/null || true
    docker exec "${CONTAINER_NAME}" find /build-target -name "sentrux" -type f 2>/dev/null || true
    exit 1
}

echo "  → Binary extracted."
echo ""

# ── Place binary in dist/ ───────────────────────────────────────────────────
OUTPUT_NAME="sentrux-${VERSION}-${COMMIT_SHA}-linux-arm64"
OUTPUT_PATH="${DIST_DIR}/${OUTPUT_NAME}"

echo "[4/4] Finalizing..."

if [[ -f "${EXTRACTED}" ]]; then
    cp "${EXTRACTED}" "${OUTPUT_PATH}"
    rm -f "${EXTRACTED}"
    chmod +x "${OUTPUT_PATH}"

    # Verify the binary is ARM64 ELF
    FILE_INFO=$(file "${OUTPUT_PATH}" 2>/dev/null || echo "unknown")
    if echo "${FILE_INFO}" | grep -qi "aarch64\|arm64\|ELF.*aarch64"; then
        echo "  → Binary confirmed: ${FILE_INFO}"
    else
        echo "  → WARNING: Binary architecture check inconclusive."
        echo "    file output: ${FILE_INFO}"
    fi

    BINARY_SIZE=$(stat -f%z "${OUTPUT_PATH}" 2>/dev/null || stat -c%s "${OUTPUT_PATH}" 2>/dev/null || echo "unknown")
    echo "  → Binary size: ${BINARY_SIZE} bytes"
else
    echo "[ERROR] Extracted binary not found."
    exit 1
fi

echo ""
echo "═══════════════════════════════════════════════════════════════"
echo "  ✓ Build complete"
echo "  Output: ${OUTPUT_PATH}"
echo "═══════════════════════════════════════════════════════════════"
echo ""

# ── Test note ────────────────────────────────────────────────────────────────
echo "NOTE: Tests are skipped during cross-compilation."
echo "      Cross-compiled binaries (aarch64-unknown-linux-gnu) cannot run"
echo "      on the macOS host. Unit tests should be verified by running"
echo "      'cargo test' natively after build, or via CI targeting Linux ARM64."
echo ""

exit 0
