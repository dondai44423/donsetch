# MinGW-w64 cross toolchain for the local windows-gnu check (win-check).
#
# A real toolchain file makes CMake run a true cross configure: with
# CMAKE_SYSTEM_NAME set, CMake computes CMAKE_CROSSCOMPILING itself. The
# -DCMAKE_CROSSCOMPILING=true that boring-sys passes does not survive
# modern CMake (the cache entry is shadowed by the variable CMake owns),
# so without this file dependency feature checks take their try_run path
# and fail on PE test binaries Linux cannot execute (google benchmark's
# regex detection is the first casualty, and BoringSSL's configure dies
# with it).
set(CMAKE_SYSTEM_NAME Windows)
set(CMAKE_SYSTEM_PROCESSOR x86_64)
set(CMAKE_C_COMPILER x86_64-w64-mingw32-gcc)
set(CMAKE_CXX_COMPILER x86_64-w64-mingw32-g++)
set(CMAKE_RC_COMPILER x86_64-w64-mingw32-windres)
set(CMAKE_FIND_ROOT_PATH /usr/x86_64-w64-mingw32)
set(CMAKE_FIND_ROOT_PATH_MODE_PROGRAM NEVER)
set(CMAKE_FIND_ROOT_PATH_MODE_LIBRARY ONLY)
set(CMAKE_FIND_ROOT_PATH_MODE_INCLUDE ONLY)
set(CMAKE_FIND_ROOT_PATH_MODE_PACKAGE ONLY)
