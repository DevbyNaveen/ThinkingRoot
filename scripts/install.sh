#!/bin/bash
set -e

# Define installation target directory
INSTALL_DIR="$HOME/.local/bin"
BIN_NAME="root"
REPO_URL="https://github.com/DevbyNaveen/releases/releases/latest/download"

echo "ThinkingRoot universal installer for macOS/Linux"
echo "=============================================="

# 1. Detect OS and architecture
OS="$(uname -s)"
ARCH="$(uname -m)"

ARTIFACT=""
TAR_BUNDLE=""

if [ "$OS" = "Linux" ]; then
    if [ "$ARCH" = "x86_64" ]; then
        ARTIFACT="root-linux-amd64"
    elif [ "$ARCH" = "aarch64" ] || [ "$ARCH" = "arm64" ]; then
        ARTIFACT="root-linux-arm64"
    else
        echo "Error: Unsupported Linux architecture '$ARCH'"
        exit 1
    fi
elif [ "$OS" = "Darwin" ]; then
    if [ "$ARCH" = "x86_64" ]; then
        TAR_BUNDLE="root-macos-amd64.tar.gz"
    elif [ "$ARCH" = "arm64" ] || [ "$ARCH" = "aarch64" ]; then
        ARTIFACT="root-macos-arm64"
    else
        echo "Error: Unsupported macOS architecture '$ARCH'"
        exit 1
    fi
else
    echo "Error: Unsupported OS '$OS'"
    exit 1
fi

mkdir -p "$INSTALL_DIR"

if [ -n "$TAR_BUNDLE" ]; then
    # Special handling for macOS Intel (TAR bundle contains root + libonnxruntime.dylib)
    echo "Downloading macOS x86_64 ONNX bundle..."
    DOWNLOAD_URL="$REPO_URL/$TAR_BUNDLE"
    TMP_DIR="$(mktemp -d)"
    curl -# -L "$DOWNLOAD_URL" -o "$TMP_DIR/$TAR_BUNDLE"
    tar -xzf "$TMP_DIR/$TAR_BUNDLE" -C "$TMP_DIR"
    
    # Move binary and dynamic library
    mv "$TMP_DIR/root" "$INSTALL_DIR/$BIN_NAME"
    # Ensure dylib and its link are moved alongside the binary so ORT binds properly
    mv "$TMP_DIR/libonnxruntime.1.23.2.dylib" "$INSTALL_DIR/" 2>/dev/null || true
    mv "$TMP_DIR/libonnxruntime.dylib" "$INSTALL_DIR/" 2>/dev/null || true
    
    rm -rf "$TMP_DIR"
else
    # Regular binary download
    echo "Downloading binary for $OS ($ARCH)..."
    DOWNLOAD_URL="$REPO_URL/$ARTIFACT"
    curl -# -L "$DOWNLOAD_URL" -o "$INSTALL_DIR/$BIN_NAME"
fi

chmod +x "$INSTALL_DIR/$BIN_NAME"
echo "✅ Downloaded ThinkingRoot binary to $INSTALL_DIR/$BIN_NAME"

# Update PATH permanently
PROFILE=""
if [ -n "$ZSH_VERSION" ] || [[ "$SHELL" == *"zsh"* ]]; then
    PROFILE="$HOME/.zshrc"
elif [ -n "$BASH_VERSION" ] || [[ "$SHELL" == *"bash"* ]]; then
    if [ "$(uname -s)" = "Darwin" ]; then
        PROFILE="$HOME/.bash_profile"
    else
        PROFILE="$HOME/.bashrc"
    fi
else
    PROFILE="$HOME/.profile"
fi

# Fallback safely if expected config file doesn't exist
if [ ! -f "$PROFILE" ]; then
    touch "$PROFILE"
fi

if ! grep -q "$INSTALL_DIR" "$PROFILE" 2>/dev/null; then
    echo "" >> "$PROFILE"
    echo "# Configured by ThinkingRoot installer" >> "$PROFILE"
    echo "export PATH=\"$INSTALL_DIR:\$PATH\"" >> "$PROFILE"
    echo "✅ Added $INSTALL_DIR to your PATH in $PROFILE"
    echo "⚠️  IMPORTANT: Please restart your terminal, or run the following command to update your current session:"
    echo "   source $PROFILE"
else
    echo "✅ $INSTALL_DIR is already in your PATH ($PROFILE)"
fi

echo "🚀 ThinkingRoot installation complete! Run 'root --help' to get started."
