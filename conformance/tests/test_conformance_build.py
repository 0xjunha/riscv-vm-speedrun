from __future__ import annotations

import importlib.util
import sys
from pathlib import Path

import pytest

CONFORMANCE = Path(__file__).resolve().parents[1]
BUILD_MODULE_NAME = "_conformance_build"
BUILD_SPEC = importlib.util.spec_from_file_location(
    BUILD_MODULE_NAME,
    CONFORMANCE / "build.py",
)
if BUILD_SPEC is None or BUILD_SPEC.loader is None:
    raise RuntimeError("could not load conformance/build.py")

build = importlib.util.module_from_spec(BUILD_SPEC)
sys.modules[BUILD_MODULE_NAME] = build
BUILD_SPEC.loader.exec_module(build)


def test_project_inputs_include_only_declared_adapter_files(tmp_path: Path) -> None:
    adapters = tmp_path / "adapters"
    documentation = adapters / "act4/README.md"
    documentation.parent.mkdir(parents=True)
    documentation.write_text("Adapter documentation.\n", encoding="utf-8")

    inputs = set(build.project_input_paths(tmp_path))
    adapter_inputs = {
        path.relative_to(adapters) for path in inputs if path.is_relative_to(adapters)
    }

    assert adapter_inputs == set(build.ADAPTER_BUILD_INPUTS)
    assert documentation not in inputs


def test_project_inputs_reject_undeclared_adapter_build_files(
    tmp_path: Path,
) -> None:
    undeclared = tmp_path / "adapters/act4/extra.h"
    undeclared.parent.mkdir(parents=True)
    undeclared.write_text("#pragma once\n", encoding="utf-8")

    with pytest.raises(ValueError, match="act4/extra.h"):
        build.project_input_paths(tmp_path)
