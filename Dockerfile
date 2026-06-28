# syntax=docker/dockerfile:1

# ─── Build stage ───
FROM rust:1.96.0-bookworm AS builder
WORKDIR /build
COPY Cargo.toml Cargo.toml
COPY Cargo.lock Cargo.lock
COPY src src

RUN cargo build --release --locked --bin sandbox-server


# ─── Runtime stage ───
#
# The AWS Lambda MicroVM managed base image. Lambda boots a Firecracker MicroVM
# from this OS, then runs this container's CMD as a long-lived server. This is
# not a Lambda custom runtime, RIE, or Runtime API image.
FROM public.ecr.aws/lambda/microvms:al2023-minimal

# Toolchain the agent's bash/python/node code expects, plus fuse + util-linux so the
# /run hook can mount the workspace with mountpoint-s3.
# Packages: git, jq, tar, gzip, unzip, which, findutils, procps-ng, util-linux,
#           python3, python3-pip, shadow-utils, ca-certificates, fuse, fuse-libs, nodejs.
RUN dnf install -y \
    git \
    jq \
    tar \
    gzip \
    unzip \
    which \
    findutils \
    procps-ng \
    util-linux \
    python3 \
    python3-pip \
    shadow-utils \
    ca-certificates \
    fuse \
    fuse-libs \
    && curl -fsSL https://rpm.nodesource.com/setup_22.x | bash - \
    && dnf install -y nodejs \
    && dnf clean all \
    && rm -rf /var/cache/dnf

# Install uv (Python package manager) system-wide (not under /root).
RUN curl -LsSf https://astral.sh/uv/install.sh | sh \
    && mv /root/.local/bin/uv /usr/local/bin/uv \
    && mv /root/.local/bin/uvx /usr/local/bin/uvx

# Install ripgrep — the harness `grep` tool shells out to `rg`. AL2023's default
# dnf repos don't carry ripgrep, so fetch the official static release binary.
ARG RIPGREP_VERSION=14.1.1
RUN ARCH=$(uname -m) \
    && if [ "$ARCH" = "aarch64" ]; then RG_TARGET="aarch64-unknown-linux-gnu"; \
       else echo "unsupported arch for ripgrep: $ARCH" && exit 1; fi \
    && curl -fsSL -o /tmp/rg.tar.gz \
       "https://github.com/BurntSushi/ripgrep/releases/download/${RIPGREP_VERSION}/ripgrep-${RIPGREP_VERSION}-${RG_TARGET}.tar.gz" \
    && tar -xzf /tmp/rg.tar.gz -C /tmp \
    && mv "/tmp/ripgrep-${RIPGREP_VERSION}-${RG_TARGET}/rg" /usr/local/bin/rg \
    && chmod +x /usr/local/bin/rg \
    && rm -rf /tmp/rg.tar.gz "/tmp/ripgrep-${RIPGREP_VERSION}-${RG_TARGET}"

# Install mountpoint-s3 (`mount-s3`). The /run lifecycle hook uses it to mount the
# namespace-scoped workspace S3 prefix at /mnt/workspaces/<namespace>.
RUN ARCH=$(uname -m) \
    && if [ "$ARCH" = "aarch64" ]; then MS3_ARCH="arm64"; \
       else echo "unsupported arch for mountpoint-s3: $ARCH" && exit 1; fi \
    && curl -fsSL -o /tmp/mount-s3.rpm \
       "https://s3.amazonaws.com/mountpoint-s3-release/latest/${MS3_ARCH}/mount-s3.rpm" \
    && dnf install -y /tmp/mount-s3.rpm \
    && dnf clean all \
    && rm -f /tmp/mount-s3.rpm

# Copy the compiled server binary.
COPY --from=builder /build/target/release/sandbox-server /usr/local/bin/sandbox-server
RUN chmod +x /usr/local/bin/sandbox-server \
    && mkdir -p /tmp/agent-workspace /mnt/workspaces

# The exec API (8080, proxied from external 443) and the lifecycle hooks (9000) must
# both be exposed — Lambda calls hooks over the guest network namespace.
EXPOSE 8080 9000

CMD ["/usr/local/bin/sandbox-server"]
