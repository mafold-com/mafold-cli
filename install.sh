#!/usr/bin/env bash
# Install + run mafold (Mafold terminal client + Claude Code agent).
#   cd ~/your-project
#   bash <(curl -fsSL https://raw.githubusercontent.com/mafold-com/mafold-cli/main/install.sh) \
#     agent --detach --token mb_xxxx
# Any args after the script are forwarded to mafold (working dir = where you run this).
set -euo pipefail
REPO="mafold-com/mafold-cli"

os="$(uname -s)"; arch="$(uname -m)"
case "$os-$arch" in
  Darwin-arm64)   target="aarch64-apple-darwin";;
  Darwin-x86_64)  target="x86_64-apple-darwin";;
  Linux-x86_64)   target="x86_64-unknown-linux-gnu";;
  *) echo "unsupported platform: $os-$arch — build from source: https://github.com/$REPO" >&2; exit 1;;
esac

command -v claude >/dev/null 2>&1 || \
  echo "⚠️  Claude Code (claude) not found on PATH — needed for 'agent'. Fix with: mafold install claude-code  (or https://claude.com/claude-code)" >&2

dir="${HOME}/.mafold"; mkdir -p "$dir"; bin="$dir/mafold"
url="https://github.com/$REPO/releases/latest/download/mafold-$target"
echo "↓ downloading mafold ($target)…"
curl -fsSL "$url" -o "$bin"
chmod +x "$bin"

# macOS (esp. Apple Silicon): a freshly-downloaded binary is often SIGKILL'd on
# its FIRST exec while AMFI validates the code signature ("zsh: killed"), and
# Sonoma+ tags it with `com.apple.provenance` which trips Gatekeeper-style
# checks. Clearing the attrs + re-applying a LOCAL ad-hoc signature makes the
# kernel trust it immediately, so `mafold login` runs on the first try.
if [ "$(uname)" = "Darwin" ]; then
  xattr -c "$bin" 2>/dev/null || true
  codesign --force --sign - "$bin" 2>/dev/null || true
fi

# Put `mafold` on PATH. Prefer a dir that is BOTH on PATH and writable (so the
# command works immediately); else ~/.local/bin with a one-line PATH hint.
on_path() { case ":$PATH:" in *":$1:"*) return 0;; *) return 1;; esac; }
bindir=""
for d in /opt/homebrew/bin /usr/local/bin "$HOME/.local/bin"; do
  if [ -d "$d" ] && [ -w "$d" ] && on_path "$d"; then bindir="$d"; break; fi
done
if [ -z "$bindir" ]; then bindir="$HOME/.local/bin"; mkdir -p "$bindir"; fi
ln -sf "$bin" "$bindir/mafold"
echo "✓ installed → $bindir/mafold"
if ! on_path "$bindir"; then
  echo "ℹ️  add to PATH (then reopen the shell):  echo 'export PATH=\"$bindir:\$PATH\"' >> ~/.zshrc"
fi

if [ "$#" -gt 0 ]; then
  exec "$bindir/mafold" "$@"
else
  echo "run:  mafold agent --detach --token mb_xxx        # works in the current folder"
fi
