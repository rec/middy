"""Structured progress for compound messages and broadcasts."""

from __future__ import annotations

from typing import TYPE_CHECKING, cast

from .midi import SendError

if TYPE_CHECKING:
    from .events import PortInfo


class BroadcastSendError(SendError):
    def __init__(
        self,
        message: str,
        completed_ports: list[PortInfo],
        failed_port: PortInfo,
        total_ports: int,
    ) -> None:
        super().__init__(message, completed_ports, failed_port, total_ports)

    @property
    def completed_ports(self) -> list[PortInfo]:
        return list(cast("list[PortInfo]", self.args[1]))

    @property
    def failed_port(self) -> PortInfo:
        return cast("PortInfo", self.args[2])

    @property
    def total_ports(self) -> int:
        return cast(int, self.args[3])

    def __str__(self) -> str:
        return str(self.args[0])


class CompoundSendError(SendError):
    def __init__(
        self, message: str, completed_messages: int | None, total_messages: int
    ) -> None:
        super().__init__(message, completed_messages, total_messages)

    @property
    def completed_messages(self) -> int | None:
        return cast(int | None, self.args[1])

    @property
    def total_messages(self) -> int:
        return cast(int, self.args[2])

    def __str__(self) -> str:
        return str(self.args[0])
