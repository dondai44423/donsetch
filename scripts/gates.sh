#!/bin/sh
# Tag-time payload gates, mirrored from release.yml so they can run
# locally before a tag ever pushes. Usage: scripts/gates.sh <platform> <dir>
# Platforms: linux-x64 | linux-arm64 | darwin-arm64 | darwin-x64 | win32-x64
# The second arg is the directory containing the built binary.

set -eu

platform="${1:?platform}"
dir="${2:?binary dir}"
root="$(cd "$(dirname "$0")/.." && pwd)"

cd "$root/$dir"

if [ "$platform" = "win32-x64" ]; then
    BIN=donsetch.exe
    NEED=25000000
    EXPECT="commit probe ok"
else
    BIN=donsetch
    case "$platform" in
        darwin-arm64) NEED=12000000; EXPECT="shared library present" ;;
        darwin-x64)   NEED=12000000; EXPECT="shared library present" ;;
        linux-x64)    NEED=15000000; EXPECT="shared library present" ;;
        linux-arm64)  NEED=15000000; EXPECT="not compiled" ;;
        *)            NEED=6000000;  EXPECT="not compiled" ;;
    esac
fi

[ -f "$BIN" ] || { echo "gates FAIL: $BIN not in $dir"; exit 1; }

SIZE=$(wc -c < "$BIN")
if [ "$SIZE" -lt "$NEED" ]; then
    echo "gates FAIL: $BIN is ${SIZE} bytes, expected >= ${NEED}"
    echo "(a binary this small cannot contain the features it was built with)"
    exit 1
fi

if [ "$platform" = "linux-x64" ]; then
    [ -f libonnxruntime.so ] || { echo "gates FAIL: libonnxruntime.so missing"; exit 1; }
    [ "$(wc -c < libonnxruntime.so)" -ge 10000000 ] || { echo "gates FAIL: libonnxruntime.so too small"; exit 1; }
    # QEMU non-AVX run when qemu-x86_64 exists (release.yml always runs it).
    if command -v qemu-x86_64 >/dev/null 2>&1; then
        rm -f ~/.cache/donsetch/avx.json 2>/dev/null || true
        OUT=$(qemu-x86_64 -cpu qemu64 "./$BIN" --version 2>&1 || true)
        [ -n "$OUT" ] || { echo "gates FAIL: binary crashed on non-AVX CPU (SIGILL)"; exit 1; }
    fi
fi

if [ "$platform" = "darwin-arm64" ] || [ "$platform" = "darwin-x64" ]; then
    [ -f libonnxruntime.dylib ] || { echo "gates FAIL: libonnxruntime.dylib missing"; exit 1; }
    [ "$(wc -c < libonnxruntime.dylib)" -ge 10000000 ] || { echo "gates FAIL: libonnxruntime.dylib too small"; exit 1; }
fi

# Version check: the binary must report the Cargo.toml version.
VERSION=$(grep -m1 '^version' "$root/Cargo.toml" | cut -d'"' -f2)
OUT=$(./"$BIN" --version 2>&1 || true)
[ -n "$OUT" ] || { echo "gates FAIL: binary produced no output"; exit 1; }
echo "$OUT" | grep -q "$VERSION" || {
    echo "gates FAIL: binary does not report version $VERSION"; echo "$OUT"; exit 1; }

# Doctor payload probe.
DOCTOR=$(./"$BIN" doctor 2>&1 || true)
echo "$DOCTOR" | grep -q "ONNX Runtime.*${EXPECT}" || {
    echo "gates FAIL: doctor did not report the expected ONNX state (${EXPECT})"
    echo "$DOCTOR" | grep -i onnx || true
    exit 1
}

echo "gates ok: ${SIZE} bytes, version ${VERSION}, ONNX probe: ${EXPECT}"
