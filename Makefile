VM0 := vm/00-python-interpreter
HARNESS_PACKAGE := rv32im-harness
HARNESS_SOURCE := harness/src
HARNESS_TESTS := harness/tests
CONFORMANCE_IMAGE := rv32im-conformance-builder:local
CONFORMANCE_BASE_IMAGE := $(shell sed -n 's/^UBUNTU_IMAGE=//p' conformance/toolchain.env)
CONFORMANCE_MANIFEST := conformance/artifacts/manifest.json

.PHONY: check conformance conformance-build conformance-check conformance-format \
	conformance-image conformance-lint conformance-reproducible \
	conformance-sources harness-format harness-lint harness-test lock-check vm0 \
	vm0-build vm0-conformance vm0-format vm0-lint vm0-test

lock-check:
	uv lock --check

vm0-build:
	./$(VM0)/build.sh

vm0-test:
	PYTHONPATH=$(VM0)/src uv run --locked pytest $(VM0)/tests

vm0-format:
	uv run --locked ruff format $(VM0)

vm0-lint:
	uv run --locked ruff format --check $(VM0)
	uv run --locked ruff check $(VM0)

vm0: vm0-build vm0-test vm0-lint

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

vm0-conformance: vm0-build conformance-check
	uv run --locked --package $(HARNESS_PACKAGE) \
		rv32im-conformance $(VM0)/out/rv32vm $(CONFORMANCE_MANIFEST)

conformance-format:
	uv run --locked ruff format conformance/build.py

conformance-lint:
	uv run --locked ruff format --check conformance/build.py
	uv run --locked ruff check conformance/build.py

conformance-reproducible: conformance-sources conformance-image
	docker run --rm --platform linux/amd64 \
		--user "$$(id -u):$$(id -g)" -e HOME=/tmp \
		-v "$(CURDIR):/repo" -w /repo $(CONFORMANCE_IMAGE) \
		python3 conformance/build.py reproduce

check: lock-check vm0 harness-lint harness-test conformance-lint vm0-conformance
