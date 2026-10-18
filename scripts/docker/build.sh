#!/usr/bin/env bash
# Сборка клиента samp-cef под Windows в Docker: client.dll, renderer.exe, loader.
#   scripts/docker/build.sh
# Результат — target/i686-pc-windows-msvc/release. Кэш cargo, MSVC CRT/SDK (xwin), DirectX SDK и
# libcef.lib остаются в томах Docker и в third_party/ — повторная сборка быстрая.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
docker build -t samp-cef-build "$ROOT/scripts/docker"
docker run --rm \
  -v "$ROOT":/src \
  -v samp-cef-cargo-registry:/usr/local/cargo/registry \
  -v samp-cef-cargo-git:/usr/local/cargo/git \
  -v samp-cef-rustup:/usr/local/rustup \
  -e XWIN_ACCEPT_LICENSE=1 \
  samp-cef-build bash -c '
    # lld-link из LLVM 14 не понимает экспорт "EXPORTAS", который выдаёт свежий rustc, —
    # берём rust-lld той же версии, что компилятор (под именем lld-link он работает как MSVC-линкер)
    mkdir -p /opt/lld
    ln -sf "$(rustc --print sysroot)/lib/rustlib/$(rustc -vV | sed -n "s/host: //p")/bin/rust-lld" /opt/lld/lld-link
    PATH=/opt/lld:$PATH bash scripts/build-client-win32.sh'
