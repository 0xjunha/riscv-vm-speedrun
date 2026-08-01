VM_LIST := vm0 vm1 vm2 vm3 vm4 vm5
BASELINE_VM := vm0
NATIVE_VM_LIST := vm4 vm5
VM_DIR_vm0 := vm_references/vm0-python-interpreter
VM_DIR_vm1 := vm_references/vm1-python-block-interpreter
VM_DIR_vm2 := vm_references/vm2-rust-interpreter
VM_DIR_vm3 := vm_references/vm3-rust-block-interpreter
VM_DIR_vm4 := vm_references/vm4-rust-jit-compiler
VM_DIR_vm5 := vm_references/vm5-rust-aot-compiler
PYTHON_VM_COMMON := vm_references/python-interpreter-common
RUST_VM_COMMON := vm_references/rust-interpreter-common
RUST_COMMON_MANIFEST := $(RUST_VM_COMMON)/Cargo.toml
RUST_COMMON_TARGET := $(RUST_VM_COMMON)/target
RUST_X86_COMPILER := vm_references/rust-x86-block-compiler
RUST_X86_COMPILER_MANIFEST := $(RUST_X86_COMPILER)/Cargo.toml
RUST_X86_COMPILER_TARGET := $(RUST_X86_COMPILER)/target
VM2_MANIFEST := $(VM_DIR_vm2)/Cargo.toml
VM2_TARGET := $(VM_DIR_vm2)/target
VM3_MANIFEST := $(VM_DIR_vm3)/Cargo.toml
VM3_TARGET := $(VM_DIR_vm3)/target
VM4_MANIFEST := $(VM_DIR_vm4)/Cargo.toml
VM4_TARGET := $(VM_DIR_vm4)/target
VM5_MANIFEST := $(VM_DIR_vm5)/Cargo.toml
VM5_TARGET := $(VM_DIR_vm5)/target
X86_TARGET := x86_64-unknown-linux-gnu
HOST_PLATFORM := $(shell uname -s)-$(shell uname -m)
RUNTIME_VM_LIST := $(VM_LIST)
ifneq ($(HOST_PLATFORM),Linux-x86_64)
RUNTIME_VM_LIST := $(filter-out $(NATIVE_VM_LIST),$(RUNTIME_VM_LIST))
endif
VM_BUILD_TARGETS := $(addsuffix -build,$(VM_LIST))
VM_RUNTIME_BUILD_TARGETS := $(addsuffix -build,$(RUNTIME_VM_LIST))
VM_TEST_TARGETS := $(addsuffix -test,$(VM_LIST))
VM_FORMAT_TARGETS := $(addsuffix -format,$(VM_LIST))
VM_LINT_TARGETS := $(addsuffix -lint,$(VM_LIST))
VM_CONFORMANCE_TARGETS := $(addsuffix -conformance,$(VM_LIST))
VM_CONTRACT_TARGETS := $(addsuffix -contract,$(VM_LIST))
VM_BENCHMARK_SMOKE_TARGETS := $(addsuffix -benchmark-smoke,$(VM_LIST))
VM_RUNTIME_CONFORMANCE_TARGETS := $(addsuffix -conformance,$(RUNTIME_VM_LIST))
VM_RUNTIME_CONTRACT_TARGETS := $(addsuffix -contract,$(RUNTIME_VM_LIST))
VM_RUNTIME_BENCHMARK_SMOKE_TARGETS := $(addsuffix -benchmark-smoke,$(RUNTIME_VM_LIST))
VM_COMPARE_ARGS = $(foreach vm,$(RUNTIME_VM_LIST),--vm $(vm)=$(VM_DIR_$(vm))/out/rv32vm)
HARNESS_PACKAGE := rv32im-harness
HARNESS_SOURCE := harness/src
HARNESS_TESTS := harness/tests
BENCHMARK_IMAGE := rv32im-benchmark-builder:local
BENCHMARK_MANIFEST := benchmarks/artifacts/manifest.json
BENCHMARK_COMPARE_OUTPUT ?= benchmarks/out/comparison.json
GCP_ENV_FILE ?= .env.gcp
CONFORMANCE_IMAGE := rv32im-conformance-builder:local
CONFORMANCE_BASE_IMAGE := $(shell sed -n 's/^UBUNTU_IMAGE=//p' conformance/toolchain.env)
CONFORMANCE_MANIFEST := conformance/artifacts/manifest.json
CONTRACT_MANIFEST := contracts/artifacts/manifest.json

.PHONY: benchmark benchmark-build benchmark-check benchmark-compare \
	benchmark-correctness benchmark-format \
	benchmark-guest-lint benchmark-image benchmark-lint benchmark-reproducible \
	benchmark-test check conformance conformance-build conformance-check \
	conformance-format conformance-image conformance-lint \
	conformance-reproducible conformance-sources conformance-test contract \
	contract-build contract-check contract-format contract-lint \
	contract-reproducible contract-test harness-format harness-lint \
	gcp-benchmark harness-test lock-check python-vm-format python-vm-lint \
	rust-vm-common-format rust-vm-common-lint rust-vm-common-test \
	rust-x86-compiler-format rust-x86-compiler-lint rust-x86-compiler-test \
	native-vm-runtime-status native-x86-test spec-check \
	vm4-platform-test vm4-x86-check \
	vm5-platform-test vm5-x86-check \
	$(VM_LIST) $(VM_BUILD_TARGETS) $(VM_TEST_TARGETS) $(VM_FORMAT_TARGETS) \
	$(VM_LINT_TARGETS) $(VM_CONFORMANCE_TARGETS) $(VM_CONTRACT_TARGETS) \
	$(VM_BENCHMARK_SMOKE_TARGETS)

lock-check:
	uv lock --check

spec-check:
	./scripts/verify-riscv-specifications.sh

define VM_RULES
$(1)-build:
	./$$(VM_DIR_$(1))/build.sh

$(1)-benchmark-smoke: $(1)-build benchmark-check
	uv run --locked --package $$(HARNESS_PACKAGE) \
		rv32im-benchmark $$(VM_DIR_$(1))/out/rv32vm $$(BENCHMARK_MANIFEST) \
		--case tiny --warmups 0 --repetitions 1 --output /dev/null

$(1)-conformance: $(1)-build conformance-check
	uv run --locked --package $$(HARNESS_PACKAGE) \
		rv32im-conformance $$(VM_DIR_$(1))/out/rv32vm $$(CONFORMANCE_MANIFEST)

$(1)-contract: $(1)-build contract-check
	uv run --locked --package $$(HARNESS_PACKAGE) \
		rv32im-contract $$(VM_DIR_$(1))/out/rv32vm $$(CONTRACT_MANIFEST)
endef

$(foreach vm,$(VM_LIST),$(eval $(call VM_RULES,$(vm))))

vm0-test: vm0-build
	PYTHONPATH=$(VM_DIR_vm0)/out uv run --locked pytest $(VM_DIR_vm0)/tests

python-vm-format:
	uv run --locked ruff format $(PYTHON_VM_COMMON)

python-vm-lint:
	uv run --locked ruff format --check $(PYTHON_VM_COMMON)
	uv run --locked ruff check $(PYTHON_VM_COMMON)

vm0-format: python-vm-format
	uv run --locked ruff format $(VM_DIR_vm0)

vm0-lint: python-vm-lint
	uv run --locked ruff format --check $(VM_DIR_vm0)
	uv run --locked ruff check $(VM_DIR_vm0)

vm0: vm0-test vm0-lint

vm1-test: vm1-build
	PYTHONPATH=$(VM_DIR_vm1)/out uv run --locked pytest $(VM_DIR_vm1)/tests

vm1-format: python-vm-format
	uv run --locked ruff format $(VM_DIR_vm1)

vm1-lint: python-vm-lint
	uv run --locked ruff format --check $(VM_DIR_vm1)
	uv run --locked ruff check $(VM_DIR_vm1)

vm1: vm1-test vm1-lint

rust-vm-common-test:
	CARGO_TARGET_DIR=$(RUST_COMMON_TARGET) cargo test --locked \
		--manifest-path $(RUST_COMMON_MANIFEST)

rust-vm-common-format:
	cargo fmt --manifest-path $(RUST_COMMON_MANIFEST)

rust-vm-common-lint:
	cargo fmt --check --manifest-path $(RUST_COMMON_MANIFEST)
	CARGO_TARGET_DIR=$(RUST_COMMON_TARGET) cargo clippy --locked \
		--manifest-path $(RUST_COMMON_MANIFEST) --all-targets -- -D warnings

rust-x86-compiler-test: rust-vm-common-test
	CARGO_TARGET_DIR=$(RUST_X86_COMPILER_TARGET) cargo test --locked \
		--manifest-path $(RUST_X86_COMPILER_MANIFEST)

rust-x86-compiler-format: rust-vm-common-format
	cargo fmt --manifest-path $(RUST_X86_COMPILER_MANIFEST)

rust-x86-compiler-lint: rust-vm-common-lint
	cargo fmt --check --manifest-path $(RUST_X86_COMPILER_MANIFEST)
	CARGO_TARGET_DIR=$(RUST_X86_COMPILER_TARGET) cargo clippy --locked \
		--manifest-path $(RUST_X86_COMPILER_MANIFEST) --all-targets -- -D warnings

vm2-test: vm2-build rust-vm-common-test
	CARGO_TARGET_DIR=$(VM2_TARGET) cargo test --locked --manifest-path $(VM2_MANIFEST)

vm2-format: rust-vm-common-format
	cargo fmt --manifest-path $(VM2_MANIFEST)

vm2-lint: rust-vm-common-lint
	cargo fmt --check --manifest-path $(VM2_MANIFEST)
	CARGO_TARGET_DIR=$(VM2_TARGET) cargo clippy --locked --manifest-path \
		$(VM2_MANIFEST) --all-targets -- -D warnings

vm2: vm2-test vm2-lint

vm3-test: vm3-build rust-vm-common-test
	CARGO_TARGET_DIR=$(VM3_TARGET) cargo test --locked --manifest-path $(VM3_MANIFEST)

vm3-format: rust-vm-common-format
	cargo fmt --manifest-path $(VM3_MANIFEST)

vm3-lint: rust-vm-common-lint
	cargo fmt --check --manifest-path $(VM3_MANIFEST)
	CARGO_TARGET_DIR=$(VM3_TARGET) cargo clippy --locked --manifest-path \
		$(VM3_MANIFEST) --all-targets -- -D warnings

vm3: vm3-test vm3-lint

vm4-test: vm4-build rust-x86-compiler-test
	CARGO_TARGET_DIR=$(VM4_TARGET) cargo test --locked --manifest-path $(VM4_MANIFEST)

vm4-format: rust-x86-compiler-format
	cargo fmt --manifest-path $(VM4_MANIFEST)

vm4-lint: rust-x86-compiler-lint
	cargo fmt --check --manifest-path $(VM4_MANIFEST)
	CARGO_TARGET_DIR=$(VM4_TARGET) cargo clippy --locked --manifest-path \
		$(VM4_MANIFEST) --all-targets -- -D warnings

vm4-x86-check:
	CARGO_TARGET_DIR=$(VM4_TARGET) cargo clippy --locked --manifest-path \
		$(VM4_MANIFEST) --target $(X86_TARGET) --all-targets -- -D warnings

native-vm-runtime-status:
ifneq ($(HOST_PLATFORM),Linux-x86_64)
	@echo "VM4 and VM5 runtime checks skipped: x86-64 Linux required"
endif

native-x86-test: benchmark-image
	./scripts/test-native-x86.sh "$(BENCHMARK_IMAGE)"

vm4-platform-test: vm4-build
ifneq ($(HOST_PLATFORM),Linux-x86_64)
	@output="$$(./$(VM_DIR_vm4)/out/rv32vm serve 2>&1)"; status=$$?; \
		test "$$status" -eq 2 && \
		test "$$output" = "rv32vm: VM4 JIT compiler requires x86-64 Linux"
endif

vm4: vm4-test vm4-lint vm4-platform-test

vm5-test: vm5-build rust-x86-compiler-test
	CARGO_TARGET_DIR=$(VM5_TARGET) cargo test --locked --manifest-path $(VM5_MANIFEST)

vm5-format: rust-x86-compiler-format
	cargo fmt --manifest-path $(VM5_MANIFEST)

vm5-lint: rust-x86-compiler-lint
	cargo fmt --check --manifest-path $(VM5_MANIFEST)
	CARGO_TARGET_DIR=$(VM5_TARGET) cargo clippy --locked --manifest-path \
		$(VM5_MANIFEST) --all-targets -- -D warnings

vm5-x86-check:
	CARGO_TARGET_DIR=$(VM5_TARGET) cargo clippy --locked --manifest-path \
		$(VM5_MANIFEST) --target $(X86_TARGET) --all-targets -- -D warnings

vm5-platform-test: vm5-build
ifneq ($(HOST_PLATFORM),Linux-x86_64)
	@output="$$(./$(VM_DIR_vm5)/out/rv32vm serve 2>&1)"; status=$$?; \
		test "$$status" -eq 2 && \
		test "$$output" = "rv32vm: VM5 AOT compiler requires x86-64 Linux"
endif

vm5: vm5-test vm5-lint vm5-platform-test

harness-test:
	PYTHONDONTWRITEBYTECODE=1 uv run --locked --package $(HARNESS_PACKAGE) \
		pytest $(HARNESS_TESTS)

harness-format:
	uv run --locked --package $(HARNESS_PACKAGE) \
		ruff format $(HARNESS_SOURCE) $(HARNESS_TESTS)

harness-lint:
	uv run --locked --package $(HARNESS_PACKAGE) \
		ruff format --check $(HARNESS_SOURCE) $(HARNESS_TESTS)
	uv run --locked --package $(HARNESS_PACKAGE) \
		ruff check $(HARNESS_SOURCE) $(HARNESS_TESTS)

benchmark-image:
	docker build --platform linux/amd64 \
		-t $(BENCHMARK_IMAGE) benchmarks

benchmark-build: benchmark-image
	docker run --rm --platform linux/amd64 \
		--user "$$(id -u):$$(id -g)" -e HOME=/tmp \
		-v "$(CURDIR):/repo" -w /repo $(BENCHMARK_IMAGE) \
		python3 benchmarks/build.py build

benchmark-check:
	PYTHONDONTWRITEBYTECODE=1 python3 benchmarks/build.py check

benchmark-correctness: vm2-build benchmark-check
	uv run --locked --package $(HARNESS_PACKAGE) \
		rv32im-benchmark $(VM_DIR_vm2)/out/rv32vm $(BENCHMARK_MANIFEST) \
		--warmups 0 --repetitions 1 --output /dev/null

benchmark-test:
	PYTHONDONTWRITEBYTECODE=1 uv run --locked pytest benchmarks/tests

benchmark-format:
	uv run --locked ruff format benchmarks/*.py benchmarks/tests

benchmark-lint:
	uv run --locked ruff format --check benchmarks/*.py benchmarks/tests
	uv run --locked ruff check benchmarks/*.py benchmarks/tests

benchmark-guest-lint: benchmark-image
	docker run --rm --platform linux/amd64 \
		--user "$$(id -u):$$(id -g)" -e HOME=/tmp \
		-v "$(CURDIR):/repo:ro" -w /repo $(BENCHMARK_IMAGE) \
		python3 benchmarks/build.py lint

benchmark-reproducible: benchmark-image
	docker run --rm --platform linux/amd64 \
		--user "$$(id -u):$$(id -g)" -e HOME=/tmp \
		-v "$(CURDIR):/repo:ro" -w /repo $(BENCHMARK_IMAGE) \
		python3 benchmarks/build.py reproduce

benchmark: benchmark-check
	@test -n "$(VM)" || (echo "usage: make benchmark VM=/path/to/rv32vm" >&2; exit 2)
	uv run --locked --package $(HARNESS_PACKAGE) \
		rv32im-benchmark "$(VM)" $(BENCHMARK_MANIFEST) $(BENCHMARK_ARGS)

benchmark-compare: native-vm-runtime-status $(VM_RUNTIME_BUILD_TARGETS) benchmark-check
	uv run --locked --package $(HARNESS_PACKAGE) \
		rv32im-benchmark-compare $(BENCHMARK_MANIFEST) \
		$(VM_COMPARE_ARGS) \
		--baseline $(BASELINE_VM) --output "$(BENCHMARK_COMPARE_OUTPUT)" \
		$(BENCHMARK_COMPARE_ARGS)

gcp-benchmark:
	./scripts/run-gcp-benchmark.sh "$(GCP_ENV_FILE)"

conformance-sources:
	PYTHONDONTWRITEBYTECODE=1 python3 conformance/build.py check-sources

conformance-image:
	docker build --platform linux/amd64 \
		--build-arg BASE_IMAGE="$(CONFORMANCE_BASE_IMAGE)" \
		-t $(CONFORMANCE_IMAGE) conformance

conformance-build: conformance-sources conformance-image
	docker run --rm --platform linux/amd64 \
		--user "$$(id -u):$$(id -g)" -e HOME=/tmp \
		-v "$(CURDIR):/repo" -w /repo $(CONFORMANCE_IMAGE) \
		python3 conformance/build.py build

conformance-check:
	PYTHONDONTWRITEBYTECODE=1 python3 conformance/build.py check

conformance: conformance-check
	@test -n "$(VM)" || (echo "usage: make conformance VM=/path/to/rv32vm" >&2; exit 2)
	uv run --locked --package $(HARNESS_PACKAGE) \
		rv32im-conformance "$(VM)" $(CONFORMANCE_MANIFEST)

conformance-format:
	uv run --locked ruff format conformance/build.py conformance/tests

conformance-lint:
	uv run --locked ruff format --check conformance/build.py conformance/tests
	uv run --locked ruff check conformance/build.py conformance/tests

conformance-test:
	PYTHONDONTWRITEBYTECODE=1 uv run --locked pytest conformance/tests

conformance-reproducible: conformance-sources conformance-image
	docker run --rm --platform linux/amd64 \
		--user "$$(id -u):$$(id -g)" -e HOME=/tmp \
		-v "$(CURDIR):/repo" -w /repo $(CONFORMANCE_IMAGE) \
		python3 conformance/build.py reproduce

contract-build: conformance-image
	docker run --rm --platform linux/amd64 \
		--user "$$(id -u):$$(id -g)" -e HOME=/tmp \
		-v "$(CURDIR):/repo" -w /repo $(CONFORMANCE_IMAGE) \
		python3 contracts/build.py build

contract-check:
	PYTHONDONTWRITEBYTECODE=1 python3 contracts/build.py check

contract: contract-check
	@test -n "$(VM)" || (echo "usage: make contract VM=/path/to/rv32vm" >&2; exit 2)
	uv run --locked --package $(HARNESS_PACKAGE) \
		rv32im-contract "$(VM)" $(CONTRACT_MANIFEST)

contract-test:
	PYTHONDONTWRITEBYTECODE=1 PYTHONPATH=contracts \
		uv run --locked pytest contracts/tests

contract-format:
	uv run --locked ruff format contracts/*.py contracts/tests

contract-lint:
	uv run --locked ruff format --check contracts/*.py contracts/tests
	uv run --locked ruff check contracts/*.py contracts/tests

contract-reproducible: conformance-image
	docker run --rm --platform linux/amd64 \
		--user "$$(id -u):$$(id -g)" -e HOME=/tmp \
		-v "$(CURDIR):/repo" -w /repo $(CONFORMANCE_IMAGE) \
		python3 contracts/build.py reproduce

check: lock-check spec-check $(VM_LIST) vm4-x86-check vm5-x86-check \
	native-vm-runtime-status harness-lint harness-test benchmark-check \
	benchmark-correctness benchmark-lint benchmark-test conformance-lint \
	conformance-test contract-lint \
	contract-test $(VM_RUNTIME_CONFORMANCE_TARGETS) $(VM_RUNTIME_CONTRACT_TARGETS) \
	$(VM_RUNTIME_BENCHMARK_SMOKE_TARGETS)
