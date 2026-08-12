"""Harbor Codex wrapper that starts the protected time-budget clock."""

from __future__ import annotations

import math
import shlex
from typing import override

from harbor.agents.installed.codex import Codex
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext


class BudgetCodex(Codex):
    """Start a protected task clock at the beginning of Codex execution."""

    def __init__(self, *args, budget_seconds: float | str, **kwargs):
        super().__init__(*args, **kwargs)
        try:
            seconds = float(budget_seconds)
        except (TypeError, ValueError) as error:
            raise ValueError("budget_seconds must be numeric") from error
        if not math.isfinite(seconds) or seconds <= 0:
            raise ValueError("budget_seconds must be positive and finite")
        self._budget_seconds = seconds

    @override
    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        seconds = format(self._budget_seconds, ".17g")
        await self.exec_as_root(
            environment,
            command=("/usr/local/bin/check-time --start " + shlex.quote(seconds)),
        )
        await super().run(instruction, environment, context)
