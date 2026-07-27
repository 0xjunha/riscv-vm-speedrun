VM0 := vm/00-python-interpreter
HARNESS_PACKAGE := rv32im-harness
HARNESS_SOURCE := harness/src
HARNESS_TESTS := harness/tests

.PHONY: check harness-format harness-lint harness-test lock-check vm0 \
	vm0-build vm0-format vm0-lint vm0-test

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

check: lock-check vm0 harness-lint harness-test
