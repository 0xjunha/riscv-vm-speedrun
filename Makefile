VM0 := vm/00-python-interpreter

.PHONY: vm0 vm0-build vm0-test vm0-format vm0-lint

vm0-build:
	./$(VM0)/build.sh

vm0-test:
	PYTHONPATH=$(VM0)/src uv run --frozen pytest $(VM0)/tests

vm0-format:
	uv run --frozen ruff format $(VM0)

vm0-lint:
	uv run --frozen ruff format --check $(VM0)
	uv run --frozen ruff check $(VM0)

vm0: vm0-build vm0-test vm0-lint
