"""Validate GCP instance and guest CPU metadata."""

import json
import sys
from pathlib import Path


def _load(path: str) -> dict[str, object]:
    return json.loads(Path(path).read_text(encoding="utf-8"))


def _resource_name(value: object) -> str:
    return str(value).rstrip("/").rsplit("/", 1)[-1]


def _mismatches(
    label: str,
    actual: dict[str, object],
    expected: dict[str, object],
) -> list[str]:
    return [
        f"{label}: {name}: expected {expected[name]!r}, got {value!r}"
        for name, value in actual.items()
        if value != expected[name]
    ]


def _instance_errors(arguments: list[str]) -> list[str]:
    path, zone, machine, platform = arguments
    instance = _load(path)
    features = instance.get("advancedMachineFeatures", {})
    scheduling = instance.get("scheduling", {})
    return _mismatches(
        "official GCP instance contract mismatch",
        {
            "zone": _resource_name(instance.get("zone", "")),
            "machine type": _resource_name(instance.get("machineType", "")),
            "CPU platform": str(instance.get("cpuPlatform", "")),
            "threads per core": str(features.get("threadsPerCore", "")),
            "maintenance policy": str(scheduling.get("onHostMaintenance", "")),
            "automatic restart": scheduling.get("automaticRestart"),
        },
        {
            "zone": zone,
            "machine type": machine,
            "CPU platform": platform,
            "threads per core": "1",
            "maintenance policy": "TERMINATE",
            "automatic restart": False,
        },
    )


def _cpu_errors(arguments: list[str]) -> list[str]:
    path, profile, model = arguments
    facts = {
        str(row.get("field", "")).rstrip(":"): str(row.get("data", ""))
        for row in _load(path).get("lscpu", [])
    }
    actual: dict[str, object] = {
        "threads per core": facts.get("Thread(s) per core", "")
    }
    expected: dict[str, object] = {"threads per core": "1"}
    if profile == "official":
        actual["CPU model"] = facts.get("Model name", "")
        expected["CPU model"] = model
    return _mismatches("GCP guest CPU contract mismatch", actual, expected)


def main() -> int:
    command, *arguments = sys.argv[1:]
    errors = {"instance": _instance_errors, "cpu": _cpu_errors}[command](arguments)
    for error in errors:
        print(error, file=sys.stderr)
    return int(bool(errors))


if __name__ == "__main__":
    raise SystemExit(main())
