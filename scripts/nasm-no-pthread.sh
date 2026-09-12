#!/bin/sh
# NASM wrapper for cross-compiling BoringSSL to x86_64-pc-windows-gnu on Linux.
#
# BoringSSL's CMakeLists links crypto against Threads::Threads. CMake 3.28's
# FindThreads leaks the -pthread interface compile option into the ASM_NASM
# language, and nasm parses "-pthread" as "-p thread" — pre-include a file
# named "thread" — failing with "unable to open include file `thread'".
# Strip the flag; everything else passes through untouched.
#
# Used via the ASM_NASM environment variable, which CMake consults when
# detecting the NASM compiler (see the win-check recipe in the Justfile).
for arg do
  shift
  [ "$arg" = "-pthread" ] || set -- "$@" "$arg"
done
exec nasm "$@"
