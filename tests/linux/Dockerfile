FROM ubuntu:24.04

ARG RUST_VERSION=1.95.0
ENV DEBIAN_FRONTEND=noninteractive
# Ubuntu minimal images normally omit /usr/share/doc, including fzf's actual
# shell integration. Keep these executable fixtures when installing fzf.
RUN echo 'path-include=/usr/share/doc/fzf/examples/*' > /etc/dpkg/dpkg.cfg.d/zz-tildy-test \
    && apt-get update && apt-get install -y --no-install-recommends \
    build-essential ca-certificates curl git zsh fzf locales \
    zsh-autosuggestions zsh-syntax-highlighting \
    python3 python3-pexpect python3-pyte python3-jsonschema \
    gcc-x86-64-linux-gnu libc6-dev-amd64-cross \
    gcc-aarch64-linux-gnu libc6-dev-arm64-cross \
    && localedef -i en_US -f UTF-8 en_US.UTF-8 \
    && rm -rf /var/lib/apt/lists/*

ENV CARGO_HOME=/opt/cargo RUSTUP_HOME=/opt/rustup
ENV PATH=/opt/cargo/bin:$PATH
RUN curl --fail --silent --show-error --location https://sh.rustup.rs -o /tmp/rustup.sh \
    && sh /tmp/rustup.sh -y --profile minimal --default-toolchain "$RUST_VERSION" --component clippy \
    && rustup target add x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu \
    && rm /tmp/rustup.sh \
    && mkdir -p /work /opt/tildy-test-plugins \
    && ln -s /usr/share/zsh-autosuggestions /opt/tildy-test-plugins/zsh-autosuggestions \
    && ln -s /usr/share/zsh-syntax-highlighting /opt/tildy-test-plugins/zsh-syntax-highlighting \
    && chown -R 1000:1000 /work /opt/cargo /opt/rustup

ENV CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc
ENV CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc
ENV TILDY_TEST_PLUGIN_DIR=/opt/tildy-test-plugins LANG=C.UTF-8
# Exercise the real bridge as well as fake-provider failure cases. Pin both
# upstream version and asset digest; never silently skip it in this image.
RUN case "$(uname -m)" in \
      aarch64) asset=aarch64-unknown-linux-musl; digest=56b9ff412bbf374d29b99e5ac09a849124cb37a0a13121e8470df32de53c1ea6 ;; \
      x86_64) asset=x86_64-unknown-linux-musl; digest=00ff3c1f8ffb59b5c15d4b44c076bcca04d92cf0055c86b916248c14f3ae714a ;; \
      *) exit 1 ;; \
    esac \
    && curl --fail --silent --show-error --location "https://github.com/starship/starship/releases/download/v1.24.2/starship-$asset.tar.gz" -o /tmp/starship.tar.gz \
    && echo "$digest  /tmp/starship.tar.gz" | sha256sum -c - \
    && tar -xzf /tmp/starship.tar.gz -C /usr/local/bin starship \
    && rm /tmp/starship.tar.gz
USER 1000:1000
WORKDIR /work
CMD ["sleep", "infinity"]
