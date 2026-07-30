"""Shared typed errors kept out of the execution hot path."""


class ElfError(ValueError):
    """The supplied file is not an ELF image accepted by the EEI."""


class GuestTrap(Exception):
    """A precise, terminal guest architectural trap."""

    __slots__ = ("cause", "pc", "value")

    def __init__(self, cause: str, pc: int, value: int) -> None:
        super().__init__(cause)
        self.cause = cause
        self.pc = pc
        self.value = value
