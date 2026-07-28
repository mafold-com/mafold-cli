#!/usr/bin/env bash
# Build mafold-core → wasm + wasm-bindgen JS, and drop into mafold-web. Run once
# after cloning / whenever the core changes (artifacts are gitignored build outputs).
set -euo pipefail
cd "$(dirname "$0")"
WEB="${1:-../mafold-web}"
CORE_DST="$WEB/src/app/app/core"

wasm-pack build --target web --release --out-dir pkg
mkdir -p "$CORE_DST" "$WEB/public"
cp pkg/mafold_core.js pkg/mafold_core.d.ts pkg/mafold_core_bg.wasm pkg/mafold_core_bg.wasm.d.ts "$CORE_DST/"
cp pkg/mafold_core_bg.wasm "$WEB/public/mafold_core_bg.wasm"
echo "✅ mafold-core → $WEB (wasm + bindings)"
