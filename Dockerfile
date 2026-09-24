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
FROM public.ecr.aws/lambda/microvms:al2023-minimal AS runtime

# Toolchain the agent's bash/python/node code expects, plus fuse + util-linux so the
# /run hook can mount the workspace with mountpoint-s3.
# Packages: git, jq, tar, gzip, unzip, which, findutils, procps-ng, util-linux,
#           python3, python3-pip, shadow-utils, ca-certificates, fuse, fuse-libs,
#           mount-s3, nodejs.
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
    mount-s3 \
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

# Copy the compiled server binary.
COPY --from=builder /build/target/release/sandbox-server /usr/local/bin/sandbox-server
RUN chmod +x /usr/local/bin/sandbox-server \
    && mkdir -p /tmp/agent-workspace /mnt/workspaces

# The exec API (8080, proxied from external 443) and the lifecycle hooks (9000) must
# both be exposed — Lambda calls hooks over the guest network namespace.
EXPOSE 8080 9000

# For plain `docker run`. Lambda ignores it and drives readiness through the /ready hook.
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s \
    CMD curl -fsS http://localhost:8080/healthz || exit 1

# Stays root: mount-s3 needs CAP_SYS_ADMIN for FUSE, and the MicroVM is the isolation boundary.
# checkov:skip=CKV_DOCKER_3: the server mounts and unmounts FUSE workspaces, which requires root
CMD ["/usr/local/bin/sandbox-server"]


# ─── Browser stage (docker build --target browser) ───
#
# Headless Chromium for screenshots, DOM dumps and CDP automation. MicroVMs are
# arm64-only and google-chrome-stable ships no linux-arm64 build, so this installs
# Playwright's Chrome Headless Shell and exposes it as `chromium`.
#
# Exec runs as root with HOME and TMPDIR on the workspace mount, so keep the profile
# on local disk:
#   chromium --no-sandbox --disable-gpu --disable-dev-shm-usage \
#     --user-data-dir="$(mktemp -d -p /tmp)" --screenshot=/tmp/shot.png https://example.com
# From Node, point playwright-core at it: chromium.launch({ executablePath: "/usr/local/bin/chromium" }).
FROM runtime AS browser

# The libraries `ldd chrome-headless-shell` reports missing on al2023-minimal, plus
# fontconfig and DejaVu Sans so pages render text (it covers Vietnamese diacritics).
ARG PLAYWRIGHT_VERSION=1.63.0
RUN dnf install -y \
    alsa-lib \
    at-spi2-core \
    atk \
    dbus-libs \
    dejavu-sans-fonts \
    expat \
    fontconfig \
    libX11 \
    libXcomposite \
    libXdamage \
    libXext \
    libXfixes \
    libXrandr \
    libxcb \
    libxkbcommon \
    mesa-libgbm \
    nspr \
    nss \
    nss-util \
    systemd-libs \
    && PLAYWRIGHT_BROWSERS_PATH=/opt/ms-playwright \
       npx -y "playwright@${PLAYWRIGHT_VERSION}" install chromium-headless-shell \
    && ln -s /opt/ms-playwright/chromium_headless_shell-*/chrome-headless-shell-linux-arm64/chrome-headless-shell \
       /usr/local/bin/chromium \
    && dnf clean all \
    && rm -rf /var/cache/dnf /root/.npm /root/.cache


# ─── Default target ───
#
# Last on purpose: CI and the MicroVM image build use the default target, so a plain
# build keeps producing the base image.
FROM runtime
