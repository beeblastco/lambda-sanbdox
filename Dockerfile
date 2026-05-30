# syntax=docker/dockerfile:1

# ─── Build stage ───
FROM rust:1.96.0-bookworm AS builder
WORKDIR /build
COPY Cargo.toml Cargo.toml
COPY Cargo.lock Cargo.lock
COPY src src

RUN cargo build --release --locked --bin bootstrap


# ─── Runtime stage ───
FROM public.ecr.aws/lambda/provided:al2023

# Pin the RIE version for reproducible builds.
ARG RIE_VERSION=1.25

# Install everything in a single RUN to reduce layers and image size.
# Packages: git, jq, tar, gzip, unzip, which, findutils, procps-ng,
#           python3, python3-pip, shadow-utils, ca-certificates, nodejs.
RUN dnf install -y \
    git \
    jq \
    tar \
    gzip \
    unzip \
    which \
    findutils \
    procps-ng \
    python3 \
    python3-pip \
    shadow-utils \
    ca-certificates \
    && curl -fsSL https://rpm.nodesource.com/setup_22.x | bash - \
    && dnf install -y nodejs \
    && dnf clean all \
    && rm -rf /var/cache/dnf

# Install uv (Python package manager) system-wide (not under /root).
RUN curl -LsSf https://astral.sh/uv/install.sh | sh \
    && mv /root/.local/bin/uv /usr/local/bin/uv \
    && mv /root/.local/bin/uvx /usr/local/bin/uvx

# Install AWS Lambda Runtime Interface Emulator (RIE) for local testing.
# RIE auto-detects Lambda vs local mode; in local mode it proxies the Runtime API on port 8080.
RUN ARCH=$(uname -m) \
    && if [ "$ARCH" = "aarch64" ]; then ARCH="arm64"; fi \
    && curl -Lo /usr/local/bin/aws-lambda-rie \
    "https://github.com/aws/aws-lambda-runtime-interface-emulator/releases/download/${RIE_VERSION}/aws-lambda-rie-${ARCH}" \
    && chmod +x /usr/local/bin/aws-lambda-rie

# Copy the compiled bootstrap binary and make it executable.
COPY --from=builder /build/target/release/bootstrap /var/runtime/bootstrap
RUN chmod +x /var/runtime/bootstrap \
    && mkdir -p /tmp/agent-workspace

# Lambda custom runtime entrypoint.
# In AWS Lambda, RIE detects the real environment and passes through to bootstrap.
# In local Docker, RIE emulates the Runtime API on port 8080.
ENTRYPOINT ["/usr/local/bin/aws-lambda-rie", "/var/runtime/bootstrap"]
