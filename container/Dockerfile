# Build leash-ipc CLI used by container-side IPC command wrappers.
FROM rust:1.85-bookworm AS leash-ipc-builder
WORKDIR /build/leash-ipc
COPY leash-ipc /build/leash-ipc
RUN cargo build --bin leash-ipc

# May workspace container
# Fedora-based environment with Python/uv, data science libs, CJK fonts, and dev tools.
FROM fedora:41

# Core system tools
RUN dnf install -y \
    bash \
    git \
    curl \
    wget \
    jq \
    ripgrep \
    fd-find \
    tree \
    less \
    python3 \
    python3-pip \
    ffmpeg \
    unzip \
    tar \
    gzip \
    bzip2 \
    xz \
    && dnf clean all

# Install prebuilt CLI binaries to avoid expensive source builds during image creation.
ARG XH_VERSION=v0.25.3
ARG JAQ_VERSION=v2.3.0
RUN set -eux; \
    arch="$(uname -m)"; \
    case "$arch" in \
      x86_64) xh_triple="x86_64-unknown-linux-musl"; jaq_triple="x86_64-unknown-linux-musl" ;; \
      aarch64|arm64) xh_triple="aarch64-unknown-linux-musl"; jaq_triple="aarch64-unknown-linux-gnu" ;; \
      *) echo "unsupported architecture: $arch" >&2; exit 1 ;; \
    esac; \
    curl -fsSL "https://github.com/ducaale/xh/releases/download/${XH_VERSION}/xh-${XH_VERSION}-${xh_triple}.tar.gz" -o /tmp/xh.tar.gz; \
    tar -xzf /tmp/xh.tar.gz -C /tmp; \
    install -m 0755 "/tmp/xh-${XH_VERSION}-${xh_triple}/xh" /usr/local/bin/xh; \
    curl -fsSL "https://github.com/01mf02/jaq/releases/download/${JAQ_VERSION}/jaq-${jaq_triple}" -o /usr/local/bin/jaq; \
    chmod +x /usr/local/bin/jaq; \
    rm -rf /tmp/xh.tar.gz "/tmp/xh-${XH_VERSION}-${xh_triple}"

# CJK fonts
RUN dnf install -y \
    google-noto-sans-cjk-fonts \
    google-noto-serif-cjk-fonts \
    && dnf clean all

# Python via uv
RUN curl -LsSf https://astral.sh/uv/install.sh | sh
ENV PATH="/root/.bun/bin:/root/.local/bin:${PATH}"

# Data science and web scraping libraries
RUN uv pip install --system \
    numpy \
    pandas \
    polars \
    pyarrow \
    duckdb \
    matplotlib \
    seaborn \
    scikit-learn \
    scipy \
    statsmodels \
    requests \
    httpx \
    beautifulsoup4 \
    pillow \
    opencv-python-headless

# Playwright client only (Chromium runs on host, connected via CDP)
RUN uv pip install --system playwright

# Node.js + CLI tooling (browser automation + ast-grep)
RUN dnf install -y nodejs npm && dnf clean all
RUN npm install -g @playwright/cli@latest @ast-grep/cli@latest

# Bun runtime
RUN curl -fsSL https://bun.sh/install | bash

# leash-ipc binary for IPC command wrappers inside container bash sessions.
COPY --from=leash-ipc-builder /build/leash-ipc/target/debug/leash-ipc /usr/local/bin/leash-ipc
RUN chmod +x /usr/local/bin/leash-ipc

# Entrypoint generates playwright-cli.json from BROWSER_CDP_ENDPOINT
COPY entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh

WORKDIR /workspace
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
CMD ["sleep", "infinity"]
