# The image every CI job runs in: Fedora, because that is what this project is
# built and run on. The host crates link against the system's alsa-lib, clang and
# glibc, and the firmware toolchain is Fedora's cargo with an Xtensa toolchain
# bolted on; testing on another distribution would test something else.
#
# GitHub offers Ubuntu, Windows and macOS runners and no Fedora runner, so the
# workflow runs an Ubuntu runner *inside* this image. The kernel and the container
# runtime stay GitHub's; the toolchain, the libc and every package a test links
# against are Fedora's.
#
#   podman build -t easellet-ci .          # the same image a person can use
#   podman run --rm -it -v "$PWD:/w:z" -w /w easellet-ci bash
#
# The tag in .github/workflows/ci.yml is a hash of this file and
# tools/requirements.txt plus tools/package-lock.json; the image job refuses to
# push a tag that does not match its own inputs.
FROM fedora:latest

# Build toolchain and the headers the host crates need:
#   alsa-lib-devel   `alsa-sys`, which the bridge links for audio
#   clang/llvm       `bindgen`, for the esp-idf-sys bindings
#   pkgconf          both of the above, and `pkg-config` probes in build scripts
# Chromium is the browser the portal harness runs in; nodejs and npm install the
# driver, which is the only Node dependency in the project. DejaVu Sans is the
# font the card renderer draws with, and the renderer's tests fail without it.
RUN dnf install -y --setopt=install_weak_deps=False \
        cargo rust clippy rustfmt \
        gcc gcc-c++ make cmake pkgconf-pkg-config \
        alsa-lib-devel openssl-devel clang clang-devel llvm-devel \
        git curl unzip tar xz which findutils diffutils \
        python3 python3-pip python3-devel \
        nodejs npm chromium dejavu-sans-fonts \
        perl-core bzip2-devel file rustup \
    && dnf clean all \
    && rm -rf /var/cache/dnf /var/cache/libdnf5

# Tools the checks use: esptool and Pillow, the two versions `scripts/check.sh`
# runs. They go into the system Python and not into a virtual environment,
# because ESP-IDF's installer creates a virtual environment of its own and
# refuses to do so while `VIRTUAL_ENV` is set. Fedora marks its Python as
# externally managed, hence the explicit flag.
# `--prefix /usr` puts them where the *system* Python looks. pip's default
# location under /usr/local is dropped by `python3 -I`, and the bridge's tests
# run tools/convert.py exactly that way.
COPY tools/requirements.txt /tmp/requirements.txt
RUN python3 -m pip install --no-cache-dir --break-system-packages --prefix /usr \
        -r /tmp/requirements.txt \
    && rm /tmp/requirements.txt
ENV RUSTUP_HOME=/opt/rustup
ENV CARGO_HOME=/opt/cargo
# The order is deliberate: the distro supplies `cargo` (a rustup shim with no
# default toolchain would only ever say "no toolchain configured"), and
# `/opt/cargo/bin` supplies `rustup`, `espup` and `ldproxy`, which live nowhere
# else.
ENV PATH="/usr/bin:/opt/cargo/bin:/usr/local/bin:/usr/local/sbin:/usr/sbin"

# The headless browser driver, without a second copy of Chromium.
COPY tools/package.json tools/package-lock.json /tmp/tools/
RUN cd /tmp/tools && npm ci --omit=dev --no-audit --no-fund \
    && mkdir -p /opt/easellet-tools \
    && cp -r node_modules package.json /opt/easellet-tools/ \
    && rm -rf /tmp/tools
ENV NODE_PATH=/opt/easellet-tools/node_modules

# rustup, without a toolchain: espup installs the Xtensa one into the workspace
# (`scripts/env.sh` points RUSTUP_HOME at `.tools/rustup`, which the firmware job
# caches), and Fedora's cargo builds the host crates. Fedora ships `rustup-init`
# and not `rustup`, so the binary has to be created here.
RUN rustup-init -y --no-modify-path --profile minimal --default-toolchain none \
    && rm -rf /opt/rustup/downloads /opt/cargo/registry

# espup and ldproxy, present but not yet used: the Xtensa toolchain and ESP-IDF
# are caches (a few gigabytes) and belong to the workflow, not to the image.
RUN cargo install --locked espup@0.19.0 ldproxy@0.3.4 \
    && rm -rf /opt/cargo/registry

# The versions this image claims to have, in the log of every job that uses it,
# so a surprise can be traced to the image that caused it.
RUN { \
        printf 'fedora=%s\n' "$(rpm -E %fedora)"; \
        printf 'rustc=%s\n' "$(/usr/bin/rustc --version)"; \
        printf 'cargo=%s\n' "$(/usr/bin/cargo --version)"; \
        printf 'python=%s\n' "$(python3 --version)"; \
        printf 'node=%s\n' "$(node --version)"; \
        printf 'chromium=%s\n' "$(chromium-browser --version)"; \
        printf 'rustup=%s\n' "$(rustup --version 2>/dev/null | head -1)"; \
        printf 'espup=%s\n' "$(espup --version)"; \
    } > /etc/easellet-ci-image
RUN cat /etc/easellet-ci-image

ENTRYPOINT ["/bin/bash"]
