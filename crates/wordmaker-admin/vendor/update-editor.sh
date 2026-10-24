#!/usr/bin/env bash
# Replaces the vendored asciidoc-editor with a release from npm.
#
#   ./update-editor.sh                       # the release pinned below
#   ./update-editor.sh 0.1.3 sha512-...      # another one
#
# The editor runs on the admin origin with the full power of the API token,
# so a release is only taken with the integrity hash it is pinned to. For a
# new release, look at what changed, take the hash from
# `npm view asciidoc-editor@<version> dist.integrity` and update the pins.
#
# The editor is vendored rather than fetched at build time so that building
# WordMaker needs neither network access nor a wasm toolchain.
set -euo pipefail

cd "$(dirname "$0")"

version=${1:-0.1.2}
integrity=${2:-sha512-z3lIzlYBne8ARk5TmkUTdAHiqruIHOxbQI6AZGXDZRKsmF+KDruA6QClFo3FUfSPxaEz1CSl+ldaZJLizSVjUQ==}
if [[ $# -eq 1 ]]; then
  echo "usage: $0 [<version> <sha512 integrity>]" >&2
  exit 2
fi
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# Downloads $1@$2 from npm into $work/node_modules/$1, after checking the
# tarball against the sha512 integrity $3 pinned here. Nothing is installed
# and no package script runs.
fetch() {
  local name=$1 version=$2 integrity=$3 tarball actual
  tarball=$(npm pack --silent --ignore-scripts --pack-destination "$work" "$name@$version")
  actual="sha512-$(openssl dgst -sha512 -binary "$work/$tarball" | openssl base64 -A)"
  if [[ $actual != "$integrity" ]]; then
    echo "error: $name@$version has integrity $actual, expected $integrity" >&2
    exit 1
  fi
  mkdir -p "$work/node_modules/$name"
  tar -xzf "$work/$tarball" -C "$work/node_modules/$name" --strip-components=1
}

fetch asciidoc-editor "$version" "$integrity"

for file in asciidoc_editor.js asciidoc_editor_bg.wasm LICENSE; do
  cp "$work/node_modules/asciidoc-editor/$file" asciidoc-editor/
done
echo "$version" > asciidoc-editor/VERSION

echo "asciidoc-editor $version vendored"
