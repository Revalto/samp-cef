# Окружение для сборки клиента samp-cef под Windows (i686-pc-windows-msvc) на macOS/Linux.
# Сама сборка — scripts/build-client-win32.sh (cargo-xwin), см. scripts/docker/build.sh.
# Docker Hub из РФ отдаёт образы не всегда — берём то же самое с зеркала Google
FROM mirror.gcr.io/library/rust:1-bookworm

RUN apt-get update \
 && apt-get install -y --no-install-recommends clang lld llvm nasm p7zip-full curl ca-certificates \
 && rm -rf /var/lib/apt/lists/*

# clang-cl, lld-link, llvm-lib, llvm-rc — без суффикса версии лежат только здесь
ENV PATH=/usr/lib/llvm-14/bin:$PATH

RUN rustup target add i686-pc-windows-msvc \
 && cargo install cargo-xwin --locked

WORKDIR /src
