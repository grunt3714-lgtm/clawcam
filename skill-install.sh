#!/usr/bin/env bash
#
# OpenClaw skill installer for clawcam.
#
# Installs the clawcam binary, YOLO model, and registers the skill
# with your OpenClaw instance.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/grunt3714-lgtm/clawcam/master/skill-install.sh | bash
#
# Options (via env vars):
#   OPENCLAW_SKILLS_DIR — OpenClaw skills directory (default: ~/.config/openclaw/skills)
#   CLAWCAM_VERSION     — specific version tag (default: latest)
#   CLAWCAM_DIR         — binary install directory (default: ~/.local/bin)
#
set -euo pipefail

REPO="grunt3714-lgtm/clawcam"
SKILLS_DIR="${OPENCLAW_SKILLS_DIR:-$HOME/.config/openclaw/skills}"
INSTALL_DIR="${CLAWCAM_DIR:-$HOME/.local/bin}"
SKILL_DIR="$SKILLS_DIR/clawcam"

info()  { printf '\033[1;34m=>\033[0m %s\n' "$*"; }
warn()  { printf '\033[1;33mwarn:\033[0m %s\n' "$*"; }
error() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

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
        armv7l|armv6l)      arch="armv7" ;;
        *)                  error "unsupported architecture: $arch" ;;
    esac

    echo "${os}-${arch}"
}

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

install_binary() {
    local platform="$1" version="$2"
    local artifact="clawcam-${platform}"
    local url="https://github.com/$REPO/releases/download/$version/${artifact}.tar.gz"

    mkdir -p "$INSTALL_DIR"

    info "downloading clawcam $version ($platform)..."
    curl -fsSL "$url" | tar xz -C /tmp
    mv "/tmp/$artifact" "$INSTALL_DIR/clawcam"
    chmod +x "$INSTALL_DIR/clawcam"
    info "binary installed to $INSTALL_DIR/clawcam"
}

install_model() {
    local version="$1"
    local model_dir="$SKILL_DIR/models"
    local model_path="$model_dir/yolov8n.onnx"

    mkdir -p "$model_dir"

    if [ -f "$model_path" ]; then
        info "YOLO model already present, skipping"
        return
    fi

    local url="https://github.com/$REPO/releases/download/$version/yolov8n.onnx"
    info "downloading YOLOv8n model..."
    curl -fsSL "$url" -o "$model_path"
    info "model installed to $model_path"
}

register_skill() {
    local version="$1"

    mkdir -p "$SKILL_DIR"

    # Download SKILL.md from the repo
    info "fetching skill manifest..."
    curl -fsSL "https://raw.githubusercontent.com/$REPO/$version/SKILL.md" \
        -o "$SKILL_DIR/SKILL.md"

    # Write skill config
    cat > "$SKILL_DIR/config.json" <<EOF
{
  "name": "clawcam",
  "version": "$version",
  "description": "AI-powered camera monitoring for Raspberry Pi",
  "emoji": "📷",
  "repo": "https://github.com/$REPO",
  "bin": "$INSTALL_DIR/clawcam",
  "model": "$SKILL_DIR/models/yolov8n.onnx",
  "requires": {
    "bins": ["clawcam"]
  },
  "installed_at": "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
}
EOF

    info "skill registered at $SKILL_DIR"
}

check_deps() {
    local missing=()

    command -v curl &>/dev/null || missing+=("curl")
    command -v tar &>/dev/null  || missing+=("tar")

    if [ ${#missing[@]} -gt 0 ]; then
        error "missing required tools: ${missing[*]}"
    fi
}

main() {
    info "clawcam — OpenClaw skill installer"
    echo ""

    check_deps

    local platform version
    platform="$(detect_platform)"
    version="$(get_version)"

    info "version: $version"
    info "platform: $platform"
    info "skill dir: $SKILL_DIR"
    echo ""

    # Step 1: Install binary
    if command -v clawcam &>/dev/null; then
        local existing
        existing="$(clawcam --version 2>/dev/null || echo 'unknown')"
        info "clawcam already installed ($existing), upgrading..."
    fi
    install_binary "$platform" "$version"

    # Step 2: Install YOLO model
    install_model "$version"

    # Step 3: Register with OpenClaw
    register_skill "$version"

    # Step 4: Verify
    echo ""
    if "$INSTALL_DIR/clawcam" --version &>/dev/null; then
        info "installed: $("$INSTALL_DIR/clawcam" --version)"
    fi

    # Check PATH
    if ! echo "$PATH" | tr ':' '\n' | grep -qx "$INSTALL_DIR"; then
        echo ""
        warn "$INSTALL_DIR is not in your PATH"
        info "add to your shell profile:"
        info "  export PATH=\"$INSTALL_DIR:\$PATH\""
    fi

    echo ""
    info "skill installed successfully"
    info ""
    info "quick start:"
    info "  clawcam device add barn-cam 192.168.1.50"
    info "  clawcam setup barn-cam --webhook http://your-host:8080/events"
    echo ""
}

main "$@"
