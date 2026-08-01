FROM rust:1.96.1-slim-bookworm@sha256:e18a79fc84dfcfc3ab5ba72290398a644c135c97eaa881447fddc354ee4701a3 AS build

ENV CARGO_NET_OFFLINE=true
WORKDIR /source
COPY vm_references vm_references
COPY benchmarks/guest benchmarks/guest
COPY benchmarks/third_party benchmarks/third_party

RUN vm_references/vm0-python-interpreter/build.sh \
    && vm_references/vm1-python-block-interpreter/build.sh \
    && vm_references/vm2-rust-interpreter/build.sh \
    && vm_references/vm3-rust-block-interpreter/build.sh \
    && vm_references/vm4-rust-jit-compiler/build.sh \
    && vm_references/vm5-rust-aot-compiler/build.sh \
    && CARGO_TARGET_DIR=/native-target cargo build \
        --frozen \
        --release \
        --manifest-path benchmarks/guest/Cargo.toml \
        --package rv32im-workloads \
        --bins \
        --target x86_64-unknown-linux-gnu \
    && mkdir /native \
    && find /native-target/x86_64-unknown-linux-gnu/release \
        -maxdepth 1 -type f -perm -111 -exec cp '{}' /native/ ';'

FROM python:3.12-slim-bookworm@sha256:d50fb7611f86d04a3b0471b46d7557818d88983fc3136726336b2a4c657aa30b

ARG SOURCE_REVISION
LABEL org.opencontainers.image.revision="$SOURCE_REVISION"

ENV LANG=C \
    LC_ALL=C \
    TZ=UTC \
    PYTHONDONTWRITEBYTECODE=1 \
    PYTHONHASHSEED=0 \
    PYTHONPATH=/opt/rv32im/harness

COPY --from=build /source/vm_references/vm0-python-interpreter/out /opt/rv32im/vms/vm0
COPY --from=build /source/vm_references/vm1-python-block-interpreter/out /opt/rv32im/vms/vm1
COPY --from=build /source/vm_references/vm2-rust-interpreter/out /opt/rv32im/vms/vm2
COPY --from=build /source/vm_references/vm3-rust-block-interpreter/out /opt/rv32im/vms/vm3
COPY --from=build /source/vm_references/vm4-rust-jit-compiler/out /opt/rv32im/vms/vm4
COPY --from=build /source/vm_references/vm5-rust-aot-compiler/out /opt/rv32im/vms/vm5
COPY --from=build /native /opt/rv32im/native
COPY harness/src /opt/rv32im/harness
COPY benchmarks/artifacts /opt/rv32im/benchmarks
COPY benchmarks/gcp/run-benchmark.sh /usr/local/bin/run-benchmark

RUN mkdir /results \
    && run-benchmark --case tiny --warmups 0 --repetitions 1 >/dev/null \
    && rm /results/comparison.json \
    && chown 65534:65534 /results

USER 65534:65534
ENTRYPOINT ["run-benchmark"]
