#!/usr/bin/env bash
# Regenerate the winget manifest for a released version.
#
#   ./winget/update-manifest.sh v0.9.102
#
# Reads the real release: the `.exe` asset must already be up (release.yml's
# build-win job publishes it), and the hash comes from the bytes GitHub is
# actually serving — never from a local build, which is how a manifest ends up
# claiming a hash nobody can reproduce.
#
# Then, to get it into winget, open a PR against microsoft/winget-pkgs with the
# generated directory copied to `manifests/m/Mafold/CLI/<version>/`:
#
#   gh repo fork microsoft/winget-pkgs --clone   # first time only
#   cp -r mafold-cli/winget/manifests/m/Mafold/CLI/<version> \
#         winget-pkgs/manifests/m/Mafold/CLI/
#   cd winget-pkgs && git checkout -b mafold-cli-<version> && git add -A
#   git commit -m "New version: Mafold.CLI version <version>" && gh pr create
#
# winget's CI validates the schema, downloads the installer, checks the hash and
# smoke-installs it; a human reviewer then merges. Budget a couple of days for
# the first submission — later ones are usually same-day.
set -euo pipefail

TAG="${1:?usage: update-manifest.sh <tag, e.g. v0.9.102>}"
VERSION="${TAG#v}"
REPO="mafold-lab/mafold-cli"
ASSET="mafold-x86_64-pc-windows-msvc.exe"
URL="https://github.com/$REPO/releases/download/$TAG/$ASSET"

here="$(cd "$(dirname "$0")" && pwd)"
out="$here/manifests/m/Mafold/CLI/$VERSION"

# The hash of what is actually being served. `--fail` so a 404 (asset not
# uploaded yet) stops here instead of hashing an error page.
echo "→ hashing $URL"
sha="$(curl -fsSL "$URL" | shasum -a 256 | awk '{print toupper($1)}')"
echo "  sha256 = $sha"

date="$(gh api "repos/$REPO/releases/tags/$TAG" --jq '.published_at' | cut -dT -f1)"

mkdir -p "$out"

cat > "$out/Mafold.CLI.yaml" <<EOF
# yaml-language-server: \$schema=https://aka.ms/winget-manifest.version.1.6.0.schema.json
PackageIdentifier: Mafold.CLI
PackageVersion: $VERSION
DefaultLocale: en-US
ManifestType: version
ManifestVersion: 1.6.0
EOF

cat > "$out/Mafold.CLI.installer.yaml" <<EOF
# yaml-language-server: \$schema=https://aka.ms/winget-manifest.installer.1.6.0.schema.json
PackageIdentifier: Mafold.CLI
PackageVersion: $VERSION
# \`portable\`: one self-contained binary, no installer to run. winget drops it
# in its own packages directory and puts a \`mafold.exe\` alias on PATH — which
# is exactly what the install.sh does on Unix, minus the shell script.
InstallerType: portable
Commands:
  - mafold
ReleaseDate: $date
Installers:
  - Architecture: x64
    # The \`.exe\` twin of \`mafold-x86_64-pc-windows-msvc\` — same bytes, and the
    # extension is why it exists: winget saves the download under the url's own
    # filename, and Windows will not run a file that has none.
    InstallerUrl: $URL
    InstallerSha256: $sha
ManifestType: installer
ManifestVersion: 1.6.0
EOF

# The locale manifest is copy-with-substitutions from the previous version: the
# prose is hand-written and must not be regenerated from a template that drifts
# out of sync with what is actually in the store listing.
prev="$(ls -1d "$here/manifests/m/Mafold/CLI"/*/ 2>/dev/null \
        | grep -v "/$VERSION/$" | sort -V | tail -1)"
if [ -n "$prev" ] && [ -f "$prev/Mafold.CLI.locale.en-US.yaml" ]; then
  sed -e "s/^PackageVersion: .*/PackageVersion: $VERSION/" \
      -e "s#releases/tag/v[0-9.]*#releases/tag/$TAG#" \
      "$prev/Mafold.CLI.locale.en-US.yaml" > "$out/Mafold.CLI.locale.en-US.yaml"
  echo "→ locale carried over from $(basename "$prev")"
else
  echo "!! no previous locale manifest — write $out/Mafold.CLI.locale.en-US.yaml by hand" >&2
fi

echo "✓ $out"
ls -1 "$out"
