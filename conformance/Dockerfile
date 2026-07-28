# check=skip=InvalidDefaultArgInFrom
# Make supplies the required, digest-pinned base image from toolchain.env.
ARG BASE_IMAGE

FROM ${BASE_IMAGE} AS toolchain

ARG DEBIAN_FRONTEND=noninteractive
# Keep emulated builds within Docker Desktop's default memory budget.
ARG BUILD_JOBS=2
COPY toolchain.env /tmp/toolchain.env

# APT still verifies signed metadata and package hashes while the minimal base
# bootstraps its missing CA bundle.
# Gitlinks still pin the content; URL overrides only select mirrors.
RUN . /tmp/toolchain.env \
    && sed -i "/Signed-By:/a Snapshot: ${UBUNTU_SNAPSHOT}" \
        /etc/apt/sources.list.d/ubuntu.sources \
    && apt-get -o Acquire::https::Verify-Peer=false update \
    && apt-get -o Acquire::https::Verify-Peer=false install -y \
        --no-install-recommends ca-certificates \
    && apt-get update \
    && apt-get install -y --no-install-recommends \
        autoconf automake autotools-dev bc bison build-essential curl flex \
        gawk gperf git libexpat-dev libglib2.0-dev libgmp-dev libmpc-dev \
        libmpfr-dev libncurses-dev libslirp-dev libtool ninja-build patchutils \
        python3 python3-tomli texinfo zlib1g-dev \
    && rm -rf /var/lib/apt/lists/*

RUN . /tmp/toolchain.env \
    && git init -q /tmp/riscv-gnu-toolchain \
    && git -C /tmp/riscv-gnu-toolchain remote add origin \
        https://github.com/riscv-collab/riscv-gnu-toolchain.git \
    && git -C /tmp/riscv-gnu-toolchain fetch -q --depth 1 origin \
        "${RISCV_GNU_TOOLCHAIN_COMMIT}" \
    && git -C /tmp/riscv-gnu-toolchain checkout -q --detach FETCH_HEAD \
    && test "$(git -C /tmp/riscv-gnu-toolchain rev-parse HEAD)" = \
        "${RISCV_GNU_TOOLCHAIN_COMMIT}" \
    && git -C /tmp/riscv-gnu-toolchain config submodule.binutils.url \
        https://gnu.googlesource.com/binutils-gdb \
    && git -C /tmp/riscv-gnu-toolchain config submodule.newlib.url \
        https://github.com/mirror/newlib-cygwin.git \
    && git -C /tmp/riscv-gnu-toolchain submodule update -q \
        --init --depth 1 binutils gcc newlib \
    && cd /tmp/riscv-gnu-toolchain \
    && ./configure --prefix=/opt/riscv --with-languages=c \
        --disable-gdb --disable-linux --enable-strip \
    && GCC_EXTRA_CONFIGURE_FLAGS="--disable-gcov --disable-lto \
        --disable-libgomp --disable-libssp --disable-libquadmath \
        --disable-decimal-float --disable-libsanitizer --disable-libvtv \
        --enable-static --disable-shared" \
       BINUTILS_TARGET_FLAGS_EXTRA="--disable-gprof --disable-gprofng --disable-nls" \
       LDFLAGS="-static -static-libgcc -static-libstdc++" \
       make -j"${BUILD_JOBS}"

FROM ${BASE_IMAGE}

ARG DEBIAN_FRONTEND=noninteractive
COPY toolchain.env /opt/conformance-toolchain.env

RUN . /opt/conformance-toolchain.env \
    && sed -i "/Signed-By:/a Snapshot: ${UBUNTU_SNAPSHOT}" \
        /etc/apt/sources.list.d/ubuntu.sources \
    && apt-get -o Acquire::https::Verify-Peer=false update \
    && apt-get -o Acquire::https::Verify-Peer=false install -y \
        --no-install-recommends ca-certificates \
    && apt-get update \
    && apt-get install -y --no-install-recommends \
        curl patch python3 \
    && rm -rf /var/lib/apt/lists/* \
    && test "$(uname -m)" = x86_64 \
    && curl -fL \
        "https://github.com/riscv/sail-riscv/releases/download/${SAIL_VERSION}/sail-riscv-Linux-x86_64.tar.gz" \
        -o /tmp/sail.tar.gz \
    && echo "${SAIL_ARCHIVE_SHA256}  /tmp/sail.tar.gz" | sha256sum -c - \
    && mkdir /opt/sail \
    && tar -xzf /tmp/sail.tar.gz -C /opt/sail --strip-components=1 \
    && rm /tmp/sail.tar.gz

COPY --from=toolchain /opt/riscv /opt/riscv

ENV PATH="/opt/riscv/bin:/opt/sail/bin:${PATH}" \
    LC_ALL=C \
    TZ=UTC \
    SOURCE_DATE_EPOCH=0 \
    PYTHONDONTWRITEBYTECODE=1

RUN . /opt/conformance-toolchain.env \
    && test "$(riscv64-unknown-elf-gcc -dumpfullversion)" = \
        "${RISCV_GCC_VERSION}" \
    && riscv64-unknown-elf-as --version | head -1 | \
        grep -F " ${RISCV_BINUTILS_VERSION}" \
    && test "$(sail_riscv_sim --version)" = "${SAIL_VERSION}"

WORKDIR /repo
