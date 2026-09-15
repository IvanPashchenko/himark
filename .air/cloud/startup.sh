#!/usr/bin/env bash
# Environment bootstrap for the himark workspace.
#
# The image is a bare Ubuntu 24.04 with no C toolchain, no headers and no
# passwordless sudo, so everything is installed into $HOME:
#
#   ~/.air-sysroot     Ubuntu .deb packages unpacked as a userspace sysroot
#                      (gcc/g++, libc headers, clang/libclang, pkg-config,
#                      fontconfig, freetype, wayland, xkbcommon, zlib, fonts)
#   ~/.air-toolchain   compiler wrappers that point gcc/clang at that sysroot
#   ~/.cargo ~/.rustup rustup with the stable toolchain
#   ~/.air-himark-env.sh  the environment, sourced from ~/.profile and ~/.bashrc
#
# Modes (AIR_STARTUP_MODE): "warmup" bakes the snapshot, so it does the slow
# cacheable work -- unpack the sysroot, install Rust, fetch the crates and run
# a full `cargo build` -- and then blocks in `healthcheck`. "task" boots from
# that snapshot, so it only re-asserts the environment and starts the agent
# host in the background before returning.

set -euo pipefail

log() { printf '[air-setup] %s\n' "$*"; }
die() { printf '[air-setup] ERROR: %s\n' "$*" >&2; exit 1; }

if [ "${AIR_STARTUP_MODE:-}" = warmup ]; then WARMUP=1; else WARMUP=; fi

REPO_ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
SYSROOT="$HOME/.air-sysroot"
TOOLCHAIN_BIN="$HOME/.air-toolchain/bin"
ENV_FILE="$HOME/.air-himark-env.sh"
APT_DIR="$HOME/.air-apt"
SYSROOT_STAMP="$SYSROOT/.air-complete"
HOST_LOG="$HOME/.air-agent-host.log"
HOST_BIN="$REPO_ROOT/target/debug/himark-agent-host"
HTTP_BIND="${HIMARK_HTTP_BIND:-0.0.0.0:4312}"
HTTP_PORT="${HTTP_BIND##*:}"

# Everything the README's "Prerequisites" and the Linux CI job install with
# apt-get. apt resolves the full dependency closure for these.
APT_PACKAGES=(
    gcc g++ make pkg-config libc6-dev
    clang libclang-dev
    libfontconfig-dev libfreetype-dev libwayland-dev libxkbcommon-dev
    zlib1g-dev fonts-dejavu-core
)

# --------------------------------------------------------------------------
# userspace sysroot
# --------------------------------------------------------------------------

install_sysroot() {
    if [ -f "$SYSROOT_STAMP" ]; then
        log "sysroot already present at $SYSROOT"
        return
    fi

    log "resolving apt dependency closure for: ${APT_PACKAGES[*]}"
    rm -rf "$APT_DIR"
    mkdir -p "$APT_DIR/state/lists/partial" "$APT_DIR/cache/archives/partial" "$APT_DIR/debs"
    : > "$APT_DIR/state/status"
    cat > "$APT_DIR/apt.conf" <<EOF
Dir::State "$APT_DIR/state";
Dir::State::status "$APT_DIR/state/status";
Dir::Cache "$APT_DIR/cache";
Dir::Cache::archives "$APT_DIR/cache/archives";
Acquire::Languages "none";
EOF
    export APT_CONFIG="$APT_DIR/apt.conf"

    apt-get -qq update
    apt-get install -y --no-install-recommends --print-uris "${APT_PACKAGES[@]}" \
        | sed -n "s/^'\(http[^']*\)'.*/\1/p" > "$APT_DIR/urls.txt"
    local count
    count=$(wc -l < "$APT_DIR/urls.txt")
    [ "$count" -gt 0 ] || die "apt resolved no download URLs; is archive.ubuntu.com reachable?"
    log "downloading $count .deb packages"
    (cd "$APT_DIR/debs" && xargs -n1 -P8 curl -sSfLO --retry 3 < "$APT_DIR/urls.txt")

    log "unpacking into $SYSROOT"
    rm -rf "$SYSROOT"
    mkdir -p "$SYSROOT"
    local deb
    for deb in "$APT_DIR"/debs/*.deb; do
        dpkg-deb -x "$deb" "$SYSROOT"
    done

    # Reproduce Ubuntu's usr-merge layout: glibc's linker scripts refer to
    # /lib/x86_64-linux-gnu/libc.so.6 and /lib64/ld-linux-x86-64.so.2, and ld
    # resolves those relative to --sysroot.
    ln -sfnT usr/lib "$SYSROOT/lib"
    ln -sfnT usr/lib64 "$SYSROOT/lib64"
    ln -sfnT usr/bin "$SYSROOT/bin"
    ln -sfnT usr/sbin "$SYSROOT/sbin"

    # dpkg-deb keeps absolute symlink targets, which would escape the sysroot
    # (fontconfig's conf.d and clang's resource dir rely on them).
    local link target
    while IFS= read -r link; do
        target=$(readlink "$link")
        case "$target" in /*) ln -sfn "$SYSROOT$target" "$link" ;; esac
    done < <(find "$SYSROOT" -type l)

    local broken
    broken=$(find "$SYSROOT" -xtype l | wc -l)
    log "sysroot unpacked ($(du -sh "$SYSROOT" | cut -f1), $broken dangling links)"

    rm -rf "$APT_DIR/debs" "$APT_DIR/cache"
    touch "$SYSROOT_STAMP"
}

# gcc/clang are relocatable but still look for headers and crt files under the
# compiled-in /usr prefix, so every invocation needs --sysroot. Rust invokes
# the linker as plain `cc`, and the `cc` crate as `cc`/`c++`, so the wrappers
# have to own those names and come first on PATH.
install_compiler_wrappers() {
    mkdir -p "$TOOLCHAIN_BIN"
    local name real
    for name in gcc cc g++ c++ clang clang++; do
        case "$name" in
            cc) real=gcc ;;
            c++) real=g++ ;;
            *) real=$name ;;
        esac
        cat > "$TOOLCHAIN_BIN/$name" <<EOF
#!/bin/sh
exec "$SYSROOT/usr/bin/$real" --sysroot="$SYSROOT" "\$@"
EOF
        chmod +x "$TOOLCHAIN_BIN/$name"
    done
    log "compiler wrappers installed in $TOOLCHAIN_BIN"
}

# --------------------------------------------------------------------------
# rust
# --------------------------------------------------------------------------

install_rust() {
    if [ -x "$HOME/.cargo/bin/rustup" ]; then
        log "rustup already present ($("$HOME/.cargo/bin/rustc" --version))"
        return
    fi
    log "installing rustup with the stable toolchain"
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --no-modify-path --profile minimal \
            --default-toolchain stable -c rustfmt -c clippy -c rust-src
    log "installed $("$HOME/.cargo/bin/rustc" --version)"
}

# CI runs the suite with cargo-nextest, so make it available too. The
# prebuilt tarball is seconds; building it from crates.io is the fallback.
install_nextest() {
    if [ -x "$HOME/.cargo/bin/cargo-nextest" ]; then
        log "cargo-nextest already present"
        return
    fi
    log "installing cargo-nextest"
    if curl -sSfL --retry 3 https://get.nexte.st/latest/linux \
        | tar -xzf - -C "$HOME/.cargo/bin" 2>/dev/null; then
        log "cargo-nextest installed from the prebuilt tarball"
    elif cargo install cargo-nextest --locked; then
        log "cargo-nextest built from crates.io"
    else
        log "WARNING: could not install cargo-nextest; use plain 'cargo test'"
    fi
}

# --------------------------------------------------------------------------
# environment
# --------------------------------------------------------------------------

# The launch runs this script as a child process, so exports made here die
# with it. Write them to a file and source that from the login shell and from
# ~/.bashrc, which is what the agent's shells actually read.
write_env_file() {
    cat > "$ENV_FILE" <<EOF
# Generated by .air/cloud/startup.sh -- himark toolchain environment.
export HIMARK_SYSROOT="$SYSROOT"
export PATH="$TOOLCHAIN_BIN:\$HOME/.cargo/bin:$SYSROOT/usr/bin:\$PATH"

# gcc/ld and the \`cc\` crate: headers come from --sysroot (see the wrappers),
# libraries and crt objects from LIBRARY_PATH.
export CC="$TOOLCHAIN_BIN/cc"
export CXX="$TOOLCHAIN_BIN/c++"
export LIBRARY_PATH="$SYSROOT/usr/lib/x86_64-linux-gnu:$SYSROOT/usr/lib\${LIBRARY_PATH:+:\$LIBRARY_PATH}"
export LD_LIBRARY_PATH="$SYSROOT/usr/lib/x86_64-linux-gnu:$SYSROOT/usr/lib\${LD_LIBRARY_PATH:+:\$LD_LIBRARY_PATH}"

# pkg-config: the .pc files carry absolute /usr paths, PKG_CONFIG_SYSROOT_DIR
# rewrites them into the sysroot.
export PKG_CONFIG_PATH="$SYSROOT/usr/lib/x86_64-linux-gnu/pkgconfig:$SYSROOT/usr/share/pkgconfig"
export PKG_CONFIG_SYSROOT_DIR="$SYSROOT"

# bindgen (rquickjs, tree-sitter) loads libclang at build time.
export LIBCLANG_PATH="$SYSROOT/usr/lib/llvm-18/lib"
export BINDGEN_EXTRA_CLANG_ARGS="--sysroot=$SYSROOT"

# Skia and the editor enumerate fonts through fontconfig at runtime.
export FONTCONFIG_PATH="$SYSROOT/etc/fonts"
export FONTCONFIG_FILE="$SYSROOT/etc/fonts/fonts.conf"
EOF
    log "wrote $ENV_FILE"

    local line="[ -f \"$ENV_FILE\" ] && . \"$ENV_FILE\"  # himark-air-env"
    local rc
    # A login shell reads only the first of these that exists.
    for rc in "$HOME/.bash_profile" "$HOME/.bash_login" "$HOME/.profile"; do
        if [ -f "$rc" ]; then break; fi
    done
    [ -f "$rc" ] || { touch "$rc"; }
    local target
    for target in "$rc" "$HOME/.bashrc"; do
        [ -f "$target" ] || touch "$target"
        if ! grep -qF 'himark-air-env' "$target"; then
            printf '\n%s\n' "$line" >> "$target"
            log "hooked $ENV_FILE into $target"
        fi
    done
}

# --------------------------------------------------------------------------
# build and run
# --------------------------------------------------------------------------

prime_build_caches() {
    cd "$REPO_ROOT"
    log "fetching crates (cargo fetch --locked)"
    cargo fetch --locked

    log "building the default members (engine, plugins, backend); this is the slow one"
    cargo build --locked

    # Not default members: the winit shell the README tells you to run, and
    # hiscript, whose rquickjs dependency is what exercises bindgen/libclang.
    log "building the linux shell and hiscript"
    cargo build --locked -p linux -p hiscript

    log "disk after the build: $(df -h "$REPO_ROOT" | awk 'NR==2 {print $4" free"}')"
}

start_agent_host() {
    if [ ! -x "$HOST_BIN" ]; then
        log "no agent host binary at $HOST_BIN yet; skipping start"
        return
    fi
    if pgrep -f 'himark-agent-host --http' >/dev/null 2>&1; then
        log "agent host already running"
        return
    fi
    local web_root_args=()
    if [ -d "$REPO_ROOT/target/web" ]; then
        web_root_args=(--web-root "$REPO_ROOT/target/web")
        log "serving the web app from target/web"
    else
        log "target/web is not built; the agent host serves AHP/WebSocket only"
    fi
    log "starting the agent host on $HTTP_BIND (log: $HOST_LOG)"
    : > "$HOST_LOG"
    ( cd "$REPO_ROOT" && nohup "$HOST_BIN" --http "$HTTP_BIND" "${web_root_args[@]}" \
        >> "$HOST_LOG" 2>&1 & echo $! > "$HOME/.air-agent-host.pid" )
}

# --------------------------------------------------------------------------
# healthcheck
# --------------------------------------------------------------------------
# Asserts the things a real task needs: the toolchain compiles and links C and
# C++ against the sysroot, pkg-config finds the native libraries, the agent
# host that every shell talks to answers HTTP on its port, and the engine's
# own tests pass (which links Skia and tree-sitter and loads fontconfig at
# runtime). Polls for readiness without a deadline; the launch owns the
# timeout. Any failure returns non-zero, which fails startup.

healthcheck() {
    # shellcheck source=/dev/null
    . "$ENV_FILE"
    cd "$REPO_ROOT"

    log "healthcheck: toolchain"
    rustc --version || return 1
    cargo --version || return 1
    cc --version | head -1 || return 1

    local probe
    probe=$(mktemp -d)
    cat > "$probe/probe.cc" <<'EOF'
#include <cstdio>
#include <string>
#include <fontconfig/fontconfig.h>
#include <xkbcommon/xkbcommon.h>
int main() {
    std::string ok = "ok";
    std::printf("fontconfig %d xkbcommon %s %s\n", FcGetVersion(),
                xkb_keysym_get_name ? "linked" : "missing", ok.c_str());
    return 0;
}
EOF
    log "healthcheck: compiling and running a C++ probe against the sysroot"
    # shellcheck disable=SC2046
    "$CXX" "$probe/probe.cc" $(pkg-config --cflags --libs fontconfig xkbcommon) \
        -o "$probe/probe" || { rm -rf "$probe"; return 1; }
    "$probe/probe" || { rm -rf "$probe"; return 1; }
    rm -rf "$probe"

    log "healthcheck: pkg-config sees the native libraries"
    pkg-config --modversion fontconfig freetype2 xkbcommon wayland-client zlib || return 1

    [ -x "$HOST_BIN" ] || { log "agent host binary missing at $HOST_BIN"; return 1; }

    log "healthcheck: waiting for the agent host to answer on port $HTTP_PORT"
    local token code waited=0
    while :; do
        token=$(sed -n 's/.*[?&]tkn=\([0-9a-f][0-9a-f]*\).*/\1/p' "$HOST_LOG" 2>/dev/null | tail -1)
        if [ -n "$token" ]; then
            code=$(curl -s -o /dev/null -m 5 --noproxy '*' -w '%{http_code}' \
                "http://127.0.0.1:$HTTP_PORT/?tkn=$token" || true)
            # 200 once target/web is built, 404 ("no web root configured")
            # otherwise; both mean the router answered an authenticated request.
            case "$code" in
                200 | 404)
                    log "agent host answered HTTP $code for an authenticated request"
                    break
                    ;;
            esac
        fi
        if ! pgrep -f 'himark-agent-host --http' >/dev/null 2>&1; then
            log "the agent host is not running; its log follows"
            tail -40 "$HOST_LOG" 2>/dev/null || true
            return 1
        fi
        waited=$((waited + 3))
        log "  still waiting for the agent host (${waited}s, token=$([ -n "$token" ] && echo found || echo pending), last code=${code:-none})"
        sleep 3
    done

    code=$(curl -s -o /dev/null -m 5 --noproxy '*' -w '%{http_code}' \
        "http://127.0.0.1:$HTTP_PORT/" || true)
    if [ "$code" != 403 ]; then
        log "expected HTTP 403 for a tokenless request, got ${code:-none}"
        return 1
    fi
    log "healthcheck: tokenless request correctly rejected with 403"

    log "healthcheck: running the engine's own tests (rope, text, intervals, documents)"
    cargo test --locked --lib -p rope -p text -p intervals -p documents || return 1

    log "healthcheck: OK"
}

# --------------------------------------------------------------------------

main() {
    log "AIR_STARTUP_MODE=${AIR_STARTUP_MODE:-unset} repo=$REPO_ROOT"
    log "disk: $(df -h "$REPO_ROOT" | awk 'NR==2 {print $4" free"}'), cpus: $(nproc)"

    install_sysroot
    install_compiler_wrappers
    install_rust
    write_env_file

    # shellcheck source=/dev/null
    . "$ENV_FILE"

    if [ -n "$WARMUP" ]; then
        install_nextest
        prime_build_caches
        start_agent_host
        log "warmup: blocking on healthcheck"
        healthcheck || die "healthcheck failed"
        log "warmup complete"
    else
        # Boot from the snapshot: refresh crates for whatever lockfile this
        # branch carries and bring the backend up, both without blocking.
        ( cd "$REPO_ROOT" && nohup cargo fetch --locked >> "$HOME/.air-cargo-fetch.log" 2>&1 & ) || true
        start_agent_host
        log "task startup done; agent host log: $HOST_LOG"
    fi
}

main "$@"
