#!/bin/sh
# Payload gates: what a shipped binary must prove before it is packaged.
# The release pipeline runs this on all five platforms; `just gates` runs it
# locally against a dist-thin build. One script, one definition: the checks used
# to live twice (YAML and here) and had drifted apart.
#
# Compile success has shipped dead payloads before (v3.3.0: the ONNX archive was
# silently absent from win32-x64 and darwin-arm64), so these read the real bytes:
# the binary is big enough to hold what it claims, the ONNX runtime ships beside
# it, `doctor` PROBES that runtime instead of printing a constant, the binary
# reports the Cargo.toml version, and on Linux it asks for no glibc newer than
# the 2.35 baseline and runs on a CPU without AVX.
#
# Usage: scripts/gates.sh <platform> <dir>
#   platform: linux-x64 | linux-arm64 | darwin-arm64 | darwin-x64 | win32-x64
#   dir:      the directory holding the built binary (repo-relative or absolute)

set -eu

platform="${1:?platform}"
dir="${2:?binary dir}"
root="$(cd "$(dirname "$0")/.." && pwd)"

fail() { echo "gates FAIL: $*"; exit 1; }

# A payload probe must never use or delete the operator's state. Doctor and the
# non-AVX probe share this disposable cache, cleared between CPUs.
gate_state=$(mktemp -d "${TMPDIR:-/tmp}/donsetch-gates.XXXXXX")
trap 'rm -rf "$gate_state"' EXIT HUP INT TERM
export DONSETCH_CACHE_DIR="$gate_state/cache"
export DONSETCH_NO_CONFIG_FILE=1
export DONSETCH_NO_UPDATE_CHECK=1
unset DONSETCH_CONFIG
export XDG_CONFIG_HOME="$gate_state/config"
export XDG_DATA_HOME="$gate_state/data"
export XDG_STATE_HOME="$gate_state/state"

case "$dir" in
    /*|[A-Za-z]:*) cd "$dir" ;;
    *) cd "$root/$dir" ;;
esac

BIN=donsetch
LIB=""
NEED=15000000
EXPECT="shared library present"
case "$platform" in
    linux-x64)    LIB=libonnxruntime.so ;;
    # PDF (core) is in; OCR and rerank are off: the official ONNX aarch64
    # prebuilt deadlocks at dlopen on native arm64.
    linux-arm64)  EXPECT="not compiled"; NEED=6000000 ;;
    darwin-arm64) LIB=libonnxruntime.dylib; NEED=12000000 ;;
    darwin-x64)   LIB=libonnxruntime.dylib; NEED=12000000 ;;
    win32-x64)    BIN=donsetch.exe; LIB=onnxruntime.dll; NEED=12000000 ;;
    *) fail "unknown platform $platform" ;;
esac

[ -f "$BIN" ] || fail "$BIN not in $(pwd)"

SIZE=$(wc -c < "$BIN")
[ "$SIZE" -ge "$NEED" ] || fail "$BIN is ${SIZE} bytes, expected >= ${NEED} (too small to hold the features it was built with)"

if [ -n "$LIB" ]; then
    [ -f "$LIB" ] || fail "$LIB missing beside the binary"
    [ "$(wc -c < "$LIB")" -ge 10000000 ] || fail "$LIB too small"
fi
if [ "$platform" = "win32-x64" ]; then
    [ -f pdfium.dll ] || fail "pdfium.dll missing beside the binary"
fi

# The binary reports the Cargo.toml version (and so the tag: a release commit
# carries both). A missed bump fails here instead of shipping assets that lie.
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)
OUT=$(./"$BIN" --version 2>&1 || true)
[ -n "$OUT" ] || fail "binary produced no output"
echo "$OUT" | grep -q "$VERSION" || fail "binary does not report version $VERSION: $OUT"

# Doctor PROBES the ONNX runtime payload.
DOCTOR=$(./"$BIN" doctor 2>&1 || true)
echo "$DOCTOR" | grep -q "ONNX Runtime.*${EXPECT}" || {
    echo "$DOCTOR" | grep -i onnx || true
    fail "doctor did not report the expected ONNX state (${EXPECT})"
}

case "$platform" in
linux-*)
    # Issue #93: a newer builder leaks its glibc requirement into the artifact
    # and every 22.04 install refuses to start. objdump reads the real symbol
    # requirements of the built bytes. Only meaningful for a binary built on the
    # 22.04 baseline runner, so a developer's own build (a newer glibc) skips it.
    if [ -n "${CI:-}" ] || [ "${GATES_GLIBC:-}" = 1 ]; then
        command -v objdump >/dev/null 2>&1 || fail "objdump not installed (binutils)"
        MAXGLIBC=$(objdump -T "$BIN" | grep -o 'GLIBC_[0-9.]*' | sed 's/GLIBC_//' | sort -V | tail -1)
        [ -n "$MAXGLIBC" ] || fail "no GLIBC versioned symbols found (objdump failed, or the binary is static)"
        TOP=$(printf '2.35\n%s\n' "$MAXGLIBC" | sort -V | tail -1)
        [ "$TOP" = "2.35" ] || fail "binary requires GLIBC_$MAXGLIBC, above the 2.35 baseline"
        echo "gates: glibc baseline ok (needs GLIBC_$MAXGLIBC <= 2.35)"
    else
        echo "gates: glibc baseline skipped (CI builds on the 22.04 baseline; GATES_GLIBC=1 forces it)"
    fi
    ;;
esac

if [ "$platform" = "linux-x64" ]; then
    # QEMU's qemu64 model has SSE2 and nothing newer: a SIGILL here means AVX
    # leaked into the base binary. The shipped ONNX runtime must also report
    # present on an SSE4.2-class CPU (Nehalem, #277).
    if command -v qemu-x86_64 >/dev/null 2>&1; then
        rm -f "$DONSETCH_CACHE_DIR/avx.json"
        QOUT=$(qemu-x86_64 -cpu qemu64 "./$BIN" --version 2>&1 || true)
        [ -n "$QOUT" ] || fail "binary crashed (SIGILL) on a non-AVX CPU (qemu64)"
        QDOC=$(qemu-x86_64 -cpu Nehalem "./$BIN" doctor 2>&1 || true)
        echo "$QDOC" | grep -q "ONNX Runtime.*shared library present" || {
            echo "$QDOC" | grep -i onnx || true
            fail "doctor on a non-AVX CPU did not report the runtime present"
        }
        echo "gates: non-AVX CPU ok (qemu64, Nehalem)"
    elif [ -n "${CI:-}" ]; then
        fail "qemu-user is not installed; the non-AVX gate cannot run in CI"
    else
        echo "gates: qemu-x86_64 not installed, non-AVX gate skipped (CI always runs it)"
    fi
fi

echo "gates ok: $platform, ${SIZE} bytes, version ${VERSION}, ONNX probe: ${EXPECT}"
