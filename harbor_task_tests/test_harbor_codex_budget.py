from __future__ import annotations

import asyncio
import sys
from pathlib import Path
from types import ModuleType, SimpleNamespace

import pytest
from conftest import load_module


def stub_package(monkeypatch: pytest.MonkeyPatch, name: str) -> ModuleType:
    package = ModuleType(name)
    package.__path__ = []
    monkeypatch.setitem(sys.modules, name, package)
    return package


@pytest.fixture
def adapter(monkeypatch: pytest.MonkeyPatch) -> ModuleType:
    for name in (
        "harbor",
        "harbor.agents",
        "harbor.agents.installed",
        "harbor.environments",
        "harbor.models",
        "harbor.models.agent",
    ):
        stub_package(monkeypatch, name)

    codex = ModuleType("harbor.agents.installed.codex")

    class Codex:
        def __init__(self, *args, **kwargs):
            self.initialized_with = (args, kwargs)
            self.root_commands: list[str] = []
            self.run_calls: list[tuple[object, ...]] = []

        async def exec_as_root(self, environment, command):
            self.root_commands.append(command)
            return SimpleNamespace(return_code=0)

        async def run(self, instruction, environment, context):
            self.run_calls.append((instruction, environment, context))

    codex.Codex = Codex
    monkeypatch.setitem(sys.modules, codex.__name__, codex)

    environments = ModuleType("harbor.environments.base")
    environments.BaseEnvironment = object
    monkeypatch.setitem(sys.modules, environments.__name__, environments)
    contexts = ModuleType("harbor.models.agent.context")
    contexts.AgentContext = object
    monkeypatch.setitem(sys.modules, contexts.__name__, contexts)

    return load_module("harbor_codex_budget", "scripts/harbor_codex_budget.py")


def test_adapter_starts_clock_before_codex(
    adapter: ModuleType,
) -> None:
    agent = adapter.BudgetCodex(
        logs_dir=Path("/logs"),
        model_name="gpt-test",
        reasoning_effort="xhigh",
        budget_seconds="21600",
    )
    environment = object()
    context = object()
    asyncio.run(agent.run("instruction", environment, context))

    assert agent.root_commands == ["/usr/local/bin/check-time --start 21600"]
    assert agent.run_calls == [("instruction", environment, context)]


@pytest.mark.parametrize("value", ["0", "nan", "infinity", "not-a-number"])
def test_adapter_rejects_invalid_budget(
    adapter: ModuleType,
    value: str,
) -> None:
    with pytest.raises(ValueError, match="budget_seconds"):
        adapter.BudgetCodex(logs_dir=Path("/logs"), budget_seconds=value)
