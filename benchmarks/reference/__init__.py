"""Independent input and expected-output models for public workloads."""

from __future__ import annotations

from collections.abc import Mapping
from types import ModuleType

from . import (
    aes,
    arithmetic,
    depthconv,
    dijkstra,
    heatshrink,
    littlefs,
    mont64,
    picojpeg,
    qrcode,
    sglib,
    sha256,
    slre,
    sort_records,
    statemate,
    streaming,
    tiny,
    ud,
    x25519,
)

_WORKLOADS = {
    "tiny": tiny,
    "arithmetic": arithmetic,
    "streaming": streaming,
    "sha256": sha256,
    "heatshrink": heatshrink,
    "depthconv": depthconv,
    "dijkstra": dijkstra,
    "sort_records": sort_records,
    "qrcode": qrcode,
    "littlefs": littlefs,
    "x25519": x25519,
    "aes": aes,
    "mont64": mont64,
    "picojpeg": picojpeg,
    "sglib": sglib,
    "slre": slre,
    "statemate": statemate,
    "ud": ud,
}


def _reference_for(workload: str) -> ModuleType:
    try:
        return _WORKLOADS[workload]
    except KeyError:
        raise ValueError(f"unknown workload: {workload}") from None


def input_for(workload: str, parameters: Mapping[str, object]) -> bytes:
    """Encode one authored case as the guest's little-endian input."""

    return _reference_for(workload).input_for(parameters)


def output_for(workload: str, data: bytes) -> bytes:
    """Compute the guest's result without executing guest code."""

    return _reference_for(workload).output_for(data)


__all__ = ["input_for", "output_for"]
