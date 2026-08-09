from __future__ import annotations

import asyncio
import importlib.util
import json
import sys
import traceback
from pathlib import Path
from typing import Any

from papermachine import WorkflowContext, _Runtime, _effect, _set_runtime

_protocol_stdout = sys.stdout
sys.stdout = sys.stderr
MAX_PROTOCOL_LINE_BYTES = 16 * 1024 * 1024
MAX_PENDING_EFFECTS = 64


class EffectClient:
    def __init__(self, reader: asyncio.StreamReader) -> None:
        self.reader = reader
        self.pending: dict[str, asyncio.Future[Any]] = {}
        self.pending_kinds: dict[str, str] = {}
        self.suspended: set[str] = set()
        self.suspend_requested = False
        self.write_lock = asyncio.Lock()
        self.failure: Exception | None = None

    async def write(self, message: dict[str, Any]) -> None:
        line = json.dumps(message, separators=(",", ":")) + "\n"
        if len(line.encode("utf-8")) > MAX_PROTOCOL_LINE_BYTES:
            raise RuntimeError(
                f"workflow protocol frame exceeds {MAX_PROTOCOL_LINE_BYTES} bytes"
            )
        async with self.write_lock:
            _protocol_stdout.write(line)
            _protocol_stdout.flush()

    async def send(self, effect_id: str, kind: str, payload: dict[str, Any]) -> Any:
        if self.failure is not None:
            raise RuntimeError(
                f"Rust workflow protocol reader already failed: {self.failure}"
            ) from self.failure
        future: asyncio.Future[Any] = asyncio.get_running_loop().create_future()
        if effect_id in self.pending:
            raise RuntimeError(f"effect is already pending: {effect_id}")
        if len(self.pending) >= MAX_PENDING_EFFECTS:
            raise RuntimeError(
                f"workflow has more than {MAX_PENDING_EFFECTS} in-flight effects"
            )
        self.pending[effect_id] = future
        self.pending_kinds[effect_id] = kind
        self.suspended.discard(effect_id)
        try:
            await self.write({"id": effect_id, "kind": kind, "payload": payload})
        except BaseException:
            self.pending.pop(effect_id, None)
            self.pending_kinds.pop(effect_id, None)
            self.suspended.discard(effect_id)
            future.cancel()
            raise
        return await future

    async def request_suspension_if_quiescent(self) -> None:
        # Let every runnable coroutine consume its latest normal response and
        # emit its next durable effect before declaring the replayable Python
        # program quiescent.
        await asyncio.sleep(0)
        await asyncio.sleep(0)
        if (
            not self.suspend_requested
            and self.pending
            and self.suspended.issuperset(self.pending)
        ):
            self.suspend_requested = True
            await self.write(
                {
                    "id": "runtime:suspend",
                    "kind": "runtime_suspend",
                    "payload": {},
                }
            )

    async def read_responses(self) -> None:
        try:
            while True:
                line = await self.reader.readline()
                if not line:
                    raise RuntimeError("Rust workflow runtime closed the protocol stream")
                if len(line) > MAX_PROTOCOL_LINE_BYTES:
                    raise RuntimeError(
                        f"workflow protocol frame exceeds {MAX_PROTOCOL_LINE_BYTES} bytes"
                    )
                if not line.endswith(b"\n"):
                    raise RuntimeError("workflow protocol frame is missing its newline")
                response = json.loads(line)
                effect_id = str(response["id"])
                future = self.pending.get(effect_id)
                if future is None or future.done():
                    continue
                if response.get("suspended") is not None:
                    self.suspended.add(effect_id)
                    await self.request_suspension_if_quiescent()
                    continue
                self.pending.pop(effect_id, None)
                effect_kind = self.pending_kinds.pop(effect_id, "")
                self.suspended.discard(effect_id)
                if response.get("ok"):
                    future.set_result(response.get("result"))
                else:
                    future.set_exception(
                        RuntimeError(str(response.get("error", "effect failed")))
                    )
                if effect_kind != "complete":
                    await self.request_suspension_if_quiescent()
        except Exception as error:
            self.failure = error
            for effect_id, future in tuple(self.pending.items()):
                if not future.done():
                    future.set_exception(
                        RuntimeError(
                            "Rust workflow protocol reader failed while waiting for "
                            f"{effect_id}: {error}"
                        )
                    )
            self.pending.clear()
            self.pending_kinds.clear()
            self.suspended.clear()
            raise


def load_workflow(source_path: Path, entrypoint: str):
    spec = importlib.util.spec_from_file_location(
        "papermachine_user_workflow", source_path
    )
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot load workflow source: {source_path}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    function = getattr(module, entrypoint, None)
    if function is None or not callable(function):
        raise RuntimeError(f"workflow entrypoint not found: {entrypoint}")
    return function


async def run() -> None:
    if len(sys.argv) != 3:
        raise RuntimeError("runner requires workflow.py and entrypoint arguments")
    protocol_reader = asyncio.StreamReader(limit=MAX_PROTOCOL_LINE_BYTES + 1)
    protocol = asyncio.StreamReaderProtocol(protocol_reader)
    transport, _ = await asyncio.get_running_loop().connect_read_pipe(
        lambda: protocol,
        sys.stdin.buffer,
    )
    initialization_line = await protocol_reader.readline()
    if not initialization_line:
        raise RuntimeError("Rust workflow runtime closed before initialization")
    if len(initialization_line) > MAX_PROTOCOL_LINE_BYTES:
        raise RuntimeError(
            f"workflow protocol frame exceeds {MAX_PROTOCOL_LINE_BYTES} bytes"
        )
    if not initialization_line.endswith(b"\n"):
        raise RuntimeError("workflow initialization frame is missing its newline")
    initialization = json.loads(initialization_line)
    client = EffectClient(protocol_reader)
    runtime = _Runtime(client.send)
    _set_runtime(runtime)
    reader = asyncio.create_task(client.read_responses())
    try:
        function = load_workflow(Path(sys.argv[1]), sys.argv[2])
        context = WorkflowContext(
            request=str(initialization["request"]),
            instructions=str(initialization.get("instructions") or ""),
            params=dict(initialization.get("params") or {}),
            workflow_id=str(initialization["workflow_id"]),
            trigger=dict(initialization.get("trigger") or {}),
            context=dict(initialization.get("context") or {}),
        )
        result = await function(context)
        await _effect("complete", {"output": result})
    finally:
        for task in tuple(runtime.tasks):
            task.cancel()
        if runtime.tasks:
            await asyncio.gather(*runtime.tasks, return_exceptions=True)
        reader.cancel()
        await asyncio.gather(reader, return_exceptions=True)
        transport.close()


def main() -> None:
    try:
        asyncio.run(run())
    except BaseException as error:
        traceback.print_exception(error, file=sys.stderr)
        raise SystemExit(1) from error


if __name__ == "__main__":
    main()
