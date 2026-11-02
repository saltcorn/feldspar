# shellcheck shell=sh
# Source from the repository root before a native cargo build on Alpine/musl:
#
#     . deploy/alpine-musl/env.sh
#     cargo build --release -p sc-cli
#
# Expects deploy/alpine-musl/setup.sh to have been run for the V8 version in
# Cargo.lock (build.sh does both). See docs/BUILD_ALPINE_MUSL.md.

_sc_v8_version="$(awk '/^name = "v8"$/ { getline; gsub(/"/, "", $3); print $3; exit }' Cargo.lock)"
_sc_v8_dir="${SC_MUSL_V8_DIR:-$HOME/.local/share/feldspar-musl-v8}/$_sc_v8_version"

# The glibc V8 archive with glibc_shim.o appended, and its matching bindings.
export RUSTY_V8_ARCHIVE="$_sc_v8_dir/librusty_v8_musl.a"
export RUSTY_V8_SRC_BINDING_PATH="$_sc_v8_dir/src_binding.rs"

# bindgen (libsqlite3-sys, …) dlopens libclang from a build script.
if [ -z "${LIBCLANG_PATH:-}" ]; then
    for _sc_d in /usr/lib/llvm*/lib; do
        [ -e "$_sc_d/libclang.so" ] && LIBCLANG_PATH="$_sc_d"
    done
    export LIBCLANG_PATH
fi

# Rust's musl target links statically by default, and a static build script
# cannot dlopen libclang ("Dynamic loading not supported"). Link dynamically
# against musl instead; the binary then needs Alpine's libc, which it has.
case "${RUSTFLAGS:-}" in
    *crt-static*) ;;
    *) export RUSTFLAGS="-C target-feature=-crt-static${RUSTFLAGS:+ $RUSTFLAGS}" ;;
esac

[ -f "$RUSTY_V8_ARCHIVE" ] ||
    echo "warning: $RUSTY_V8_ARCHIVE missing; run deploy/alpine-musl/setup.sh" >&2

unset _sc_v8_version _sc_v8_dir _sc_d
