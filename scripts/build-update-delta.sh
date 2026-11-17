#!/usr/bin/env bash
# Emit a verified patch from the previous stable release, when it saves bytes.
set -euo pipefail

asset=$1
new_payload=$2
version=${GITHUB_REF_NAME#v}
if [[ ! "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    exit 0
fi

previous=$(gh release list --limit 30 --exclude-drafts --exclude-pre-releases --json tagName |
    python -c '
import json, re, sys

def key(version):
    return tuple(map(int, version.split(".")))

current = key(sys.argv[1])
tags = (release["tagName"] for release in json.load(sys.stdin))
versions = [tag[1:] for tag in tags if re.fullmatch(r"v[0-9]+\.[0-9]+\.[0-9]+", tag)]
print(max((v for v in versions if key(v) < current), key=key, default=""))
' "$version")
if [[ -z "$previous" ]]; then
    exit 0
fi

old_asset=$asset
if [[ "$asset" == *.AppImage ]]; then
    old_asset="shob-${previous}-x86_64.AppImage"
fi
if ! gh release view "v$previous" --json assets --jq '.assets[].name' | grep -Fx "$old_asset" >/dev/null; then
    exit 0
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
gh release download "v$previous" --pattern "$old_asset" --dir "$work"
old_payload="$work/$old_asset"
if [[ "$asset" == *.tar.gz ]]; then
    tar xzf "$old_payload" -C "$work"
    old_payload="$work/${old_asset%.tar.gz}/shob"
fi

cargo install gpui-updater-release --version 0.4.4 --locked --features cli,delta
delta="$asset.from-$previous.delta"
gpui-updater-release delta create --old "$old_payload" --new "$new_payload" \
    --from "$previous" --to "$version" --out "$delta"
if (( $(wc -c < "$delta") >= $(wc -c < "$asset") )); then
    rm "$delta"
elif [[ -n "${GITHUB_OUTPUT:-}" ]]; then
    printf 'file=%s\n' "$delta" >> "$GITHUB_OUTPUT"
fi
