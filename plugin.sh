#!/bin/sh
here="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
sys="$(uname -s)"
case "$sys" in
MINGW* | MSYS* | CYGWIN*) rel="x86_64-pc-windows-msvc/gray-irc.exe" ;;
Darwin*)
    case "$(uname -m)" in
    arm64) rel="aarch64-apple-darwin/gray-irc" ;;
    *) rel="x86_64-apple-darwin/gray-irc" ;;
    esac ;;
*)
    case "$(uname -m)" in
    aarch64 | arm64) rel="aarch64-unknown-linux-musl/gray-irc" ;;
    *) rel="x86_64-unknown-linux-musl/gray-irc" ;;
    esac ;;
esac
if [ -x "$here/$rel" ]; then exec "$here/$rel" "$@"; fi
for cand in "$here/target/release/gray-irc.exe" "$here/target/release/gray-irc" "$here/target/debug/gray-irc.exe" "$here/target/debug/gray-irc"; do
    if [ -x "$cand" ]; then exec "$cand" "$@"; fi
done
echo "gray-irc binary not found in $here" >&2
exit 1
