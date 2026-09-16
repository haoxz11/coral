# coral 镜像打包
# 构建方式：本机 cargo-zigbuild 交叉编译出目标架构二进制，镜像只做打包（容器内不再编译）
# 运行：debian:bookworm-slim + openssh-client（git-sync 需要：gix 的 SSH 传输
# 调用系统 ssh 命令——distroless 无 ssh 二进制会 "Failed to invoke program ssh"）
#
# 用法（在仓库根目录）：
#   rustup target add x86_64-unknown-linux-gnu
#   cargo zigbuild --release --target x86_64-unknown-linux-gnu
#   podman build -f deploy/Dockerfile -t coral:latest .
#
# 默认打包 x86_64；构建其他架构时通过 --build-arg TARGET_TRIPLE=<rust triple> 覆盖
# 注意不能用 TARGETARCH，那是 podman/docker 自动注入的预定义变量

FROM debian:bookworm-slim
ARG TARGET_TRIPLE=x86_64-unknown-linux-gnu
# 国内网络无法直连 deb.debian.org，apt 前先换阿里云镜像源
# bookworm 的源在 /etc/apt/sources.list.d/debian.sources（deb822 格式）
RUN sed -i 's|deb.debian.org|mirrors.aliyun.com|g; s|security.debian.org|mirrors.aliyun.com|g' \
        /etc/apt/sources.list.d/debian.sources \
    && apt-get update \
    && apt-get install -y --no-install-recommends openssh-client ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY target/${TARGET_TRIPLE}/dist/coral /coral
# 容器默认配置路径；实际部署用 configMap 挂载覆盖
ENV CORAL_CONFIG=/etc/coral/coral.toml
ENTRYPOINT ["/coral"]
CMD ["--config", "/etc/coral/coral.toml"]
