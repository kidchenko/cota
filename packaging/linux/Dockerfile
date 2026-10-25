# A Linux to build and try Cota in, from a Mac or anywhere else:
#
#   docker build -t cota-linux packaging/linux
#   docker run --rm -v "$PWD":/src -v cota-target:/target cota-linux cargo test
#   docker run --rm -v "$PWD":/src -v cota-target:/target cota-linux ./build.sh --package
#
# The -dev packages are what the build links against, the same ones CI
# installs. Xvfb is for drawing the panel headless, xwd and ImageMagick for
# screenshots of it, and notify-send for the notifications.
# `--build-arg UBUNTU=22.04` builds where releases are built.
ARG UBUNTU=24.04
FROM ubuntu:${UBUNTU}

RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        build-essential ca-certificates curl pkg-config bc file rpm dpkg-dev \
        libgtk-3-dev libayatana-appindicator3-dev libssl-dev \
        xvfb xauth dbus-x11 x11-apps imagemagick libnotify-bin fonts-cantarell fonts-dejavu-core python3 \
    && rm -rf /var/lib/apt/lists/*

ENV RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH CARGO_TARGET_DIR=/target
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal \
        --default-toolchain stable --component clippy,rustfmt

WORKDIR /src
