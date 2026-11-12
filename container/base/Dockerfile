# May workspace base image: OS, language runtimes and libraries shared by
# every workspace layer. Fedora stays at 41; every other version is pinned.
FROM fedora:41

# Core system tools (ripgrep/jq/ast-grep come from the aither baseline tools
# staged by the workspace layer, not from dnf).
RUN dnf install -y \
    bash \
    git \
    curl \
    wget \
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

# Python via uv, installer and interpreter packages both pinned.
ARG UV_VERSION=0.13.6
RUN curl -LsSf "https://astral.sh/uv/${UV_VERSION}/install.sh" | sh
ENV PATH="/root/.bun/bin:/root/.local/bin:${PATH}"
COPY requirements.txt /tmp/requirements.txt
RUN uv pip install --system -r /tmp/requirements.txt

# Bun runtime, pinned to an explicit release.
ARG BUN_VERSION=bun-v1.4.4
RUN curl -fsSL https://bun.sh/install | bash -s "${BUN_VERSION}"

# Playwright CLI only, pinned (Chromium runs on host, connected via CDP).
RUN dnf install -y nodejs && dnf clean all
RUN bun add --global @playwright/cli@0.1.23
