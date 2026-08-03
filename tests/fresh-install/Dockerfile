ARG TEST_IMAGE=ubuntu:24.04

FROM rust:bookworm AS builder

ARG TARGETARCH
WORKDIR /source
COPY . .

RUN --mount=type=cache,id=tmux-agent-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=tmux-agent-target-${TARGETARCH},target=/source/target \
    set -eux; \
    cargo build --locked --release; \
    target=$(rustc -vV | sed -n 's/^host: //p'); \
    version=$(scripts/check-version); \
    release_dir="/release/v${version}"; \
    scripts/package-release "$target" target/release/tmux-agent "$release_dir"; \
    cd "$release_dir"; \
    sha256sum tmux-agent-*.tar.gz >SHA256SUMS

FROM ${TEST_IMAGE}

ARG TARGETARCH
ENV DEBIAN_FRONTEND=noninteractive \
    LANG=C.UTF-8 \
    TERM=xterm-256color \
    TMUX_AGENT_EXPECTED_ARCH=${TARGETARCH}

RUN apt-get update \
    && apt-get install --yes --no-install-recommends \
        ca-certificates \
        coreutils \
        curl \
        git \
        procps \
        python3 \
        tar \
        tmux \
        util-linux \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --home-dir "/home/user" --create-home --shell /bin/bash newcomer

COPY --from=builder /release /srv/releases
COPY . /opt/tmux-agent

RUN chmod 0755 \
        /opt/tmux-agent/bin/tmux-agent \
        /opt/tmux-agent/scripts/bootstrap \
        /opt/tmux-agent/scripts/doctor \
        /opt/tmux-agent/scripts/install \
        /opt/tmux-agent/scripts/launch-popup \
        /opt/tmux-agent/scripts/uninstall \
        /opt/tmux-agent/tests/fresh-install/container-test.sh \
        /opt/tmux-agent/tmux-agent.tmux \
    && chown -R newcomer:newcomer "/home/user"

USER newcomer
WORKDIR /home/user/
ENTRYPOINT ["/opt/tmux-agent/tests/fresh-install/container-test.sh"]
