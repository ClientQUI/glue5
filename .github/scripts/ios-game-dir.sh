#!/usr/bin/env bash
# Locates the game directory for an iOS CI build.
#
# The repository does not contain GTA V. A checkout has the files only when
# GLUE5_GAME_URL points at a tarball or zip of the game directory (the folder
# that contains b/<build>/game.wasm). GLUE5_GAME_TOKEN, when set, is sent as
# a bearer token. Prints the directory that contains b/ to stdout.
set -euo pipefail

dest=${1:-game}
mkdir -p "$dest"

if [ -n "${GLUE5_GAME_URL:-}" ]; then
    echo "Fetching game files from GLUE5_GAME_URL"
    archive=$(mktemp)
    curl_args=(-fL --retry 3 --retry-delay 2 -o "$archive")
    if [ -n "${GLUE5_GAME_TOKEN:-}" ]; then
        curl_args+=(-H "Authorization: Bearer ${GLUE5_GAME_TOKEN}")
    fi
    curl "${curl_args[@]}" "$GLUE5_GAME_URL"
    case "$GLUE5_GAME_URL" in
        *.zip) unzip -q "$archive" -d "$dest" ;;
        *) tar -xf "$archive" -C "$dest" ;;
    esac
    rm -f "$archive"
fi

# BSD find (macos-latest) has no -quit. Stop at the first match.
wasm=
while IFS= read -r candidate; do
    wasm=$candidate
    break
done < <(find "$dest" -type f -path '*/b/*/game.wasm' -print)
if [ -z "$wasm" ]; then
    echo "::error title=Game files are not in this repository::Set the GLUE5_GAME_URL secret to a tarball or zip of the game directory (b/<build>/game.wasm, b/<build>/shaders, data). This repo does not ship GTA V, so the iOS IPA job stops here until that URL is set. The supported wasm build id is 8b0b5899ed."
    cat <<'EOF' >&2

This build needs the supported game files, and this repository does not contain them.

Glue 5 does not ship GTA V. The iOS app cannot be compiled until game.wasm is
supplied to CI:

  1. Pack the game directory (the folder that contains b/<build>/game.wasm,
     b/<build>/shaders and data/). The archive root may be that folder, or
     one directory above it. .tar, .tar.gz and .zip URLs are accepted.
  2. Set the repository secret GLUE5_GAME_URL to an https URL for that archive.
     For a private URL, set GLUE5_GAME_TOKEN as well; it is sent as a bearer token.
  3. Re-run the "iOS IPA" workflow (Actions → iOS IPA → Run workflow).

The supported game.wasm is build 8b0b5899ed, the id crates/glue5-game checks.
A different wasm is refused unless GLUE5_ALLOW_UNKNOWN_BUILD=1, which this
workflow does not set.

CI stops before installing the toolchain, because there is nothing to compile.
EOF
    exit 1
fi

# game.wasm is <root>/b/<id>/game.wasm.
root=$(cd "$(dirname "$wasm")/../.." && pwd)
echo "$root"
