# Docker Hub repository to build and push. Edit to your own namespace.
repo := "fabiop85/bore"
tag := "dev-opus-48"

# Dedicated buildx builder. The docker-container driver is required both for
# writing build outputs to the filesystem and for multi-arch images.
builder := "bore-builder"

# Apple Silicon target CPU for the macOS build. Current Rust/LLVM toolchains do
# not yet know "apple-m5", so this uses the newest available CPU (M-series
# optimizations); bump to "apple-m5" once your toolchain supports it.
macos_target_cpu := "apple-m4"

# Minimum Android API level for the android-arm64 build.
android_api := "24"

# Show the available recipes.
default:
    @just --list

# Ensure the buildx builder exists (created on first use).
_builder:
    docker buildx inspect {{builder}} >/dev/null 2>&1 || \
        docker buildx create --name {{builder}} --driver docker-container --bootstrap

# One-time on a fresh host: register QEMU emulators so arm64 can build on amd64.
setup-qemu:
    docker run --privileged --rm tonistiigi/binfmt --install all

# Build the linux/amd64 binary into ./bin/bore-amd64.
build-amd64: _builder
    mkdir -p bin
    docker buildx build --builder {{builder}} --platform linux/amd64 \
        -f Dockerfile --output type=local,dest=bin .
    mv bin/bore bin/bore-amd64
    @echo "built -> bin/bore-amd64"

# Build the linux/arm64 binary into ./bin/bore-arm64 (emulated; run `just setup-qemu` once).
build-arm64: _builder
    mkdir -p bin
    docker buildx build --builder {{builder}} --platform linux/arm64 \
        -f Dockerfile --output type=local,dest=bin .
    mv bin/bore bin/bore-arm64
    @echo "built -> bin/bore-arm64"

# Build the macOS Apple Silicon binary into ./bin/bore-macos-arm64 (cross via zig).
macos-m5: _builder
    mkdir -p bin
    docker buildx build --builder {{builder}} \
        -f docker/Dockerfile.cross \
        --build-arg TARGET=aarch64-apple-darwin \
        --build-arg BIN=bore \
        --build-arg RUSTFLAGS="-C target-cpu={{macos_target_cpu}}" \
        --output type=local,dest=bin .
    mv bin/bore bin/bore-macos-arm64
    @echo "built -> bin/bore-macos-arm64"

# Build the Windows amd64 binary into ./bin/bore-windows-amd64.exe (cross via zig).
windows-amd64: _builder
    mkdir -p bin
    docker buildx build --builder {{builder}} \
        -f docker/Dockerfile.cross \
        --build-arg TARGET=x86_64-pc-windows-gnu \
        --build-arg BIN=bore.exe \
        --output type=local,dest=bin .
    mv bin/bore.exe bin/bore-windows-amd64.exe
    @echo "built -> bin/bore-windows-amd64.exe"

# Build the Android arm64 binary into ./bin/bore-android-arm64 (cross via the NDK).
android-arm64: _builder
    mkdir -p bin
    docker buildx build --builder {{builder}} \
        -f docker/Dockerfile.android \
        --build-arg ANDROID_API={{android_api}} \
        --output type=local,dest=bin .
    mv bin/bore bin/bore-android-arm64
    @echo "built -> bin/bore-android-arm64"

# Build all architecture binaries (Linux amd64/arm64, macOS, Windows, Android).
build: build-amd64 build-arm64 macos-m5 windows-amd64 android-arm64

# Run `docker login` first and set `repo` above.
# Build and push a multi-arch (amd64 + arm64) image to Docker Hub.
push: _builder
    docker buildx build --builder {{builder}} --platform linux/amd64,linux/arm64 \
        -f Dockerfile -t {{repo}}:{{tag}} --push .

push_amd64: _builder
    docker buildx build --builder {{builder}} --platform linux/amd64 \
        -f Dockerfile -t {{repo}}:{{tag}} --push .