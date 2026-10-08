#!/usr/bin/env bash
# Replaces the vendored asciidoc-editor with a release from npm.
#
#   ./update-editor.sh          # the latest release
#   ./update-editor.sh 0.1.2    # a particular one
#
# The editor is vendored rather than fetched at build time so that building
# WordMaker needs neither network access nor a wasm toolchain.
set -euo pipefail

cd "$(dirname "$0")"

version=${1:-$(npm view asciidoc-editor version)}
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

npm pack --silent --pack-destination "$work" "asciidoc-editor@$version" >/dev/null
tar -xzf "$work"/asciidoc-editor-*.tgz -C "$work"

for file in asciidoc_editor.js asciidoc_editor_bg.wasm LICENSE; do
  cp "$work/package/$file" asciidoc-editor/
done
echo "$version" > asciidoc-editor/VERSION

echo "asciidoc-editor $version vendored"
