# 本地打 Linux 镜像：zigbuild 交叉编译 + podman 打包（容器内不编译）
# 用法：bash bin/image.sh [rust-target-triple]，默认 x86_64-unknown-linux-gnu
set -euo pipefail
TARGET="${1:-x86_64-unknown-linux-gnu}"
rustup target add "$TARGET"
cargo zigbuild --release --target "$TARGET"
podman build --platform linux/amd64 --build-arg TARGET_TRIPLE="$TARGET" -f deploy/Dockerfile -t coral:latest .
