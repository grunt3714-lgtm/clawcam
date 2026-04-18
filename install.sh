#!/usr/bin/env bash
#
# ClawCam installer — downloads the latest release binary and YOLO model.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/grunt3714-lgtm/clawcam/master/install.sh | bash
#
# Options (via env vars):
#   CLAWCAM_VERSION   — specific version tag (default: latest)
#   CLAWCAM_DIR       — install directory (default: ~/.local/bin)
#   CLAWCAM_MODEL_DIR — model directory (default: ~/.local/share/clawcam)
#
set -euo pipefail

REPO="grunt3714-lgtm/clawcam"
INSTALL_DIR="${CLAWCAM_DIR:-$HOME/.local/bin}"
MODEL_DIR="${CLAWCAM_MODEL_DIR:-$HOME/.local/share/clawcam}"

info()  { printf '\033[1;34m=>\033[0m %s\n' "$*"; }
error() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

# Detect platform
detect_platform() {
    local os arch
    os="$(uname -s | tr '[:upper:]' '[:lower:]')"
    arch="$(uname -m)"

    case "$os" in
        linux)  os="linux" ;;
        darwin) os="darwin" ;;
        *)      error "unsupported OS: $os" ;;
    esac

    case "$arch" in
        x86_64|amd64)       arch="amd64" ;;
        aarch64|arm64)      arch="arm64" ;;
        armv7l|armv6l)      arch="armv7" ;;  # Pi 3/Zero 2
        *)                  error "unsupported architecture: $arch" ;;
    esac

    echo "${os}-${arch}"
}

# Get latest release tag
get_version() {
    if [ -n "${CLAWCAM_VERSION:-}" ]; then
        echo "$CLAWCAM_VERSION"
        return
    fi

    local tag
    if command -v gh &>/dev/null; then
        tag="$(gh release view --repo "$REPO" --json tagName -q .tagName 2>/dev/null)" || true
    fi

    if [ -z "${tag:-}" ]; then
        tag="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
            | grep '"tag_name"' | head -1 | cut -d'"' -f4)"
    fi

    [ -n "$tag" ] || error "could not determine latest release"
    echo "$tag"
}

main() {
    local platform version artifact url

    platform="$(detect_platform)"
    version="$(get_version)"
    artifact="clawcam-${platform}"

    info "installing clawcam $version for $platform"

    # Create directories
    mkdir -p "$INSTALL_DIR" "$MODEL_DIR"

    # Download binary
    url="https://github.com/$REPO/releases/download/$version/${artifact}.tar.gz"
    info "downloading $url"
    curl -fsSL "$url" | tar xz -C /tmp
    mv "/tmp/$artifact" "$INSTALL_DIR/clawcam"
    chmod +x "$INSTALL_DIR/clawcam"
    info "installed binary to $INSTALL_DIR/clawcam"

    # Download YOLO model
    local model_path="$MODEL_DIR/yolov8n.onnx"
    if [ -f "$model_path" ]; then
        info "model already exists at $model_path, skipping"
    else
        url="https://github.com/$REPO/releases/download/$version/yolov8n.onnx"
        info "downloading YOLO model..."
        curl -fsSL "$url" -o "$model_path"
        info "installed model to $model_path"
    fi

    # Copy model to project dir for setup deployments
    mkdir -p "$(pwd)/models" 2>/dev/null || true
    if [ -d "$(pwd)/models" ] && [ ! -f "$(pwd)/models/yolov8n.onnx" ]; then
        cp "$model_path" "$(pwd)/models/yolov8n.onnx" 2>/dev/null || true
    fi

    # Check PATH
    if ! echo "$PATH" | tr ':' '\n' | grep -qx "$INSTALL_DIR"; then
        info ""
        info "add to your shell profile:"
        info "  export PATH=\"$INSTALL_DIR:\$PATH\""
        info ""
    fi

    # Verify
    if "$INSTALL_DIR/clawcam" --version &>/dev/null; then
        info "$("$INSTALL_DIR/clawcam" --version) installed successfully"
    else
        info "binary installed (run 'clawcam --version' to verify)"
    fi

    echo ""
    info "quick start:"
    info "  clawcam device add my-cam 192.168.1.50"
    info "  clawcam setup my-cam --webhook http://your-host:8080/events"
}

main "$@"
