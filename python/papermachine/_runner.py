from __future__ import annotations

import asyncio
import importlib.util
import json
import sys
import traceback
import uuid
from pathlib import Path
from typing import Any

from papermachine import WorkflowContext, _Runtime, _set_runtime

_protocol_stdout = sys.stdout
sys.stdout = sys.stderr


class EffectClient:
    def __init__(self, reader: asyncio.StreamReader) -> None:
        self.reader = reader
        self.pending: dict[str, asyncio.Future[Any]] = {}
        self.write_lock = asyncio.Lock()

    async def send(self, kind: str, payload: dict[str, Any]) -> Any:
        effect_id = str(uuid.uuid4())
        future: asyncio.Future[Any] = asyncio.get_running_loop().create_future()
        self.pending[effect_id] = future
        async with self.write_lock:
            _protocol_stdout.write(
                json.dumps({"id": effect_id, "kind": kind, "payload": payload}, separators=(",", ":"))
                + "\n"
            )
            _protocol_stdout.flush()
        return await future

    async def read_responses(self) -> None:
        while True:
            line = await self.reader.readline()
            if not line:
                raise RuntimeError("Rust workflow runtime closed the protocol stream")
            response = json.loads(line)
            future = self.pending.pop(str(response["id"]), None)
            if future is None or future.done():
                continue
            if response.get("ok"):
                future.set_result(response.get("result"))
            else:
                future.set_exception(RuntimeError(str(response.get("error", "effect failed"))))


def load_workflow(source_path: Path, entrypoint: str):
    spec = importlib.util.spec_from_file_location("papermachine_user_workflow", source_path)
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
    protocol_reader = asyncio.StreamReader()
    protocol = asyncio.StreamReaderProtocol(protocol_reader)
    transport, _ = await asyncio.get_running_loop().connect_read_pipe(
        lambda: protocol,
        sys.stdin.buffer,
    )
    initialization_line = await protocol_reader.readline()
    if not initialization_line:
        raise RuntimeError("Rust workflow runtime closed before initialization")
    initialization = json.loads(initialization_line)
    client = EffectClient(protocol_reader)
    runtime = _Runtime(client.send)
    _set_runtime(runtime)
    reader = asyncio.create_task(client.read_responses())
    try:
        function = load_workflow(Path(sys.argv[1]), sys.argv[2])
        context = WorkflowContext(
            objective=str(initialization["objective"]),
            input=dict(initialization.get("input") or {}),
            workflow_id=str(initialization["workflow_id"]),
        )
        result = await function(context)
        await client.send("complete", {"output": result})
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
