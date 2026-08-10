"""The user-facing PaperMachine workflow DSL.

Workflow code uses normal Python control flow. Every operation that changes
research state is emitted as an effect and executed by the Rust runtime.
"""

from __future__ import annotations

import asyncio
import contextvars
import inspect
import json
from collections.abc import Awaitable, Callable
from dataclasses import dataclass, field
from typing import Any, TypeVar, get_origin

_runtime: _Runtime | None = None
T = TypeVar("T")
ACCESS_PROFILES = {
    "model_only",
    "read_only",
    "workspace",
    "research",
    "full_access",
}


class HumanMessage(str):
    """A string answer whose durable HumanRequest provenance is preserved."""

    request_id: str

    def __new__(cls, content: str, request_id: str) -> HumanMessage:
        value = str.__new__(cls, content)
        value.request_id = request_id
        return value


@dataclass
class _EffectCursor:
    path: tuple[str, ...]
    next_slot: int = 0

    def reserve(self, label: str) -> tuple[str, ...]:
        slot = self.next_slot
        self.next_slot += 1
        return (*self.path, f"{label}:{slot}")


_effect_cursor: contextvars.ContextVar[_EffectCursor | None] = contextvars.ContextVar(
    "papermachine_effect_cursor",
    default=None,
)
def _current_effect_cursor() -> _EffectCursor:
    cursor = _effect_cursor.get()
    if cursor is None:
        cursor = _EffectCursor(("root",))
        _effect_cursor.set(cursor)
    return cursor


def _reserve_effect_path(label: str) -> tuple[str, ...]:
    return _current_effect_cursor().reserve(label)


def _effect_id(path: tuple[str, ...], kind: str) -> str:
    return "/".join((*path, kind))


async def _run_in_effect_branch(awaitable: Awaitable[T], path: tuple[str, ...]) -> T:
    token = _effect_cursor.set(_EffectCursor(path))
    try:
        return await awaitable
    finally:
        _effect_cursor.reset(token)


def _normalize_access(value: str) -> str:
    if not isinstance(value, str) or value not in ACCESS_PROFILES:
        expected = ", ".join(sorted(ACCESS_PROFILES))
        raise ValueError(f"Agent access must be one of: {expected}")
    return value


def workflow(**metadata: Any) -> Callable[[Callable[..., Any]], Callable[..., Any]]:
    def decorate(function: Callable[..., Any]) -> Callable[..., Any]:
        function.__papermachine_workflow__ = dict(metadata)
        return function

    return decorate


class _ActionDescriptor:
    def __init__(
        self,
        function: Callable[..., Any],
        prompt: str | None = None,
        search_context_size: str | None = None,
        reasoning_effort: str | None = None,
        finalize: str | None = None,
        tools: list[str] | None = None,
    ) -> None:
        if search_context_size not in {None, "low", "medium", "high"}:
            raise ValueError(
                "action search_context_size must be one of: low, medium, high"
            )
        if reasoning_effort not in {
            None,
            "none",
            "low",
            "medium",
            "high",
            "xhigh",
            "max",
        }:
            raise ValueError(
                "action reasoning_effort must be one of: none, low, medium, high, xhigh, max"
            )
        if finalize not in {None, "always", "after_search"}:
            raise ValueError("action finalize must be one of: always, after_search")
        self.function = function
        self.name = function.__name__
        self.prompt = (prompt or inspect.getdoc(function) or self.name).strip()
        self.signature = inspect.signature(function)
        self.human_message_parameter = _human_message_parameter(function)
        self.return_kind = _json_return_kind(function)
        self.response_format = _response_format(self.name, self.return_kind)
        self.search_context_size = search_context_size
        self.reasoning_effort = reasoning_effort
        self.finalize = finalize
        self.tools = _normalize_action_tools(tools if tools is not None else [])

    def __get__(self, instance: Agent | None, owner: type[Agent]) -> Any:
        if instance is None:
            return self

        def call(*args: Any, **kwargs: Any) -> _ActionCall:
            bound = self.signature.bind(instance, *args, **kwargs)
            arguments = {
                key: value for key, value in bound.arguments.items() if key != "self"
            }
            human_message: HumanMessage | None = None
            if self.human_message_parameter is not None:
                candidate = arguments.get(self.human_message_parameter)
                if not isinstance(candidate, HumanMessage):
                    raise TypeError(
                        f"action {self.name} parameter {self.human_message_parameter!r} "
                        "must receive a HumanMessage returned by ask_human()"
                    )
                human_message = candidate
                arguments[self.human_message_parameter] = str(candidate)
            unexpected = [
                key
                for key, value in arguments.items()
                if isinstance(value, HumanMessage)
            ]
            if unexpected:
                raise TypeError(
                    "HumanMessage arguments must use a parameter annotated as HumanMessage: "
                    + ", ".join(unexpected)
                )
            prompt = self.prompt
            if self.return_kind is not None:
                prompt = (
                    f"{prompt}\n\nReturn only valid JSON with a top-level "
                    f"{self.return_kind}; do not use Markdown fences or add commentary outside JSON."
                )
            return _ActionCall(
                instance,
                self.name,
                prompt,
                arguments,
                self.return_kind,
                self.response_format,
                self.search_context_size,
                self.reasoning_effort,
                self.finalize,
                self.tools,
                self.human_message_parameter,
                human_message.request_id if human_message is not None else None,
            )

        return call


def action(
    argument: Any = None,
    *,
    search_context_size: str | None = None,
    reasoning_effort: str | None = None,
    finalize: str | None = None,
    tools: list[str] | None = None,
) -> Any:
    if callable(argument):
        return _ActionDescriptor(
            argument,
            search_context_size=search_context_size,
            reasoning_effort=reasoning_effort,
            finalize=finalize,
            tools=tools,
        )
    if argument is not None and not isinstance(argument, str):
        raise TypeError("action prompt must be a string")

    def decorate(function: Callable[..., Any]) -> _ActionDescriptor:
        return _ActionDescriptor(
            function,
            argument,
            search_context_size,
            reasoning_effort,
            finalize,
            tools,
        )

    return decorate


def _human_message_parameter(function: Callable[..., Any]) -> str | None:
    try:
        annotations = inspect.get_annotations(function, eval_str=True)
    except (NameError, TypeError):
        annotations = function.__annotations__
    parameters = [
        name for name, annotation in annotations.items() if annotation is HumanMessage
    ]
    if len(parameters) > 1:
        raise TypeError("an action may declare at most one HumanMessage parameter")
    return parameters[0] if parameters else None


class Agent:
    system_prompt = ""
    role = "research agent"
    model = ""
    skills: list[str] = []
    access = "research"

    def __init__(
        self,
        name: str | None = None,
        *,
        role: str | None = None,
        system_prompt: str | None = None,
        model: str | None = None,
        skills: list[str] | None = None,
        access: str | None = None,
    ) -> None:
        self.name = name or type(self).__name__
        self.role = role if role is not None else type(self).role
        self.system_prompt = (
            system_prompt if system_prompt is not None else type(self).system_prompt
        )
        self.model = model if model is not None else type(self).model
        self.skills = list(skills if skills is not None else type(self).skills)
        self.access = _normalize_access(
            access if access is not None else type(self).access
        )
        self._effect_path = _reserve_effect_path("agent")
        self._remote_lock = asyncio.Lock()
        self._remote: dict[str, Any] | None = None

    async def _ensure_remote(self) -> dict[str, Any]:
        async with self._remote_lock:
            if self._remote is None:
                self._remote = await _effect(
                    "create_agent",
                    {
                        "class_name": type(self).__name__,
                        "name": self.name,
                        "role": self.role,
                        "system_prompt": self.system_prompt,
                        "model": self.model,
                        "skills": self.skills,
                        "access": self.access,
                    },
                    effect_id=_effect_id(self._effect_path, "create_agent"),
                )
                self.access = self._remote.get("access", self.access)
        return self._remote

    async def set_access(self, access: str) -> None:
        access = _normalize_access(access)
        remote = await self._ensure_remote()
        result = await _effect(
            "set_agent_access",
            {
                "agent_instance_id": remote["agent_instance_id"],
                "access": access,
            },
        )
        self.access = result["access"]

def _json_return_kind(function: Callable[..., Any]) -> str | None:
    try:
        annotation = inspect.get_annotations(function, eval_str=True).get("return")
    except (NameError, TypeError):
        annotation = function.__annotations__.get("return")
    origin = get_origin(annotation) or annotation
    return {
        dict: "object",
        list: "array",
        bool: "boolean",
        int: "integer",
        float: "number",
    }.get(origin)


def _response_format(name: str, return_kind: str | None) -> dict[str, Any] | None:
    if return_kind is None:
        return None
    schema: dict[str, Any] = {"type": return_kind}
    if return_kind == "array":
        schema["items"] = {}
    return {
        "name": f"{name}_result",
        "schema": schema,
        "strict": False,
    }


class _ActionCall(Awaitable[Any]):
    def __init__(
        self,
        agent: Agent,
        name: str,
        prompt: str,
        arguments: dict[str, Any],
        return_kind: str | None,
        response_format: dict[str, Any] | None,
        search_context_size: str | None,
        reasoning_effort: str | None,
        finalize: str | None,
        tools: list[str],
        human_message_parameter: str | None,
        human_request_id: str | None,
    ) -> None:
        self.agent = agent
        self.name = name
        self.prompt = prompt
        self.arguments = arguments
        self.return_kind = return_kind
        self.response_format = response_format
        self.search_context_size = search_context_size
        self.reasoning_effort = reasoning_effort
        self.finalize = finalize
        self.tools = list(tools)
        self.human_message_parameter = human_message_parameter
        self.human_request_id = human_request_id
        self.action_invocation_id: str | None = None
        self._started = False
        self._completed = False

    def __await__(self):  # type: ignore[no-untyped-def]
        if self._started:
            raise RuntimeError(f"action {self.name} may be awaited only once")
        self._started = True
        return self._run().__await__()

    async def _run(self) -> Any:
        remote = await self.agent._ensure_remote()
        result = await self._invoke(remote, use_human_message=True)
        invocation_id = result.get("action_invocation_id")
        if not isinstance(invocation_id, str) or not invocation_id:
            raise RuntimeError("invoke_action did not return action_invocation_id")
        self.action_invocation_id = invocation_id
        try:
            hosted_search_calls = int(result.get("hosted_search_calls_used", 0))
        except (TypeError, ValueError):
            hosted_search_calls = 0
        should_finalize = self.finalize == "always" or (
            self.finalize == "after_search" and hosted_search_calls > 0
        )
        if should_finalize:
            result = await self._invoke(
                remote,
                action_name=f"{self.name}_finalize",
                prompt=(
                    "Turn the immediately preceding action result into the actual final "
                    "deliverable requested by that action. The preceding result may be "
                    "research notes or progress narration rather than an answer. Do not do "
                    "new research or call tools. Return only the complete, self-contained "
                    "deliverable in the original requested format; preserve verified evidence, "
                    "source URLs, exact values, and material limitations."
                ),
                arguments={
                    "original_action": self.name,
                    "finalization_policy": self.finalize,
                },
                tools_enabled=False,
                search_context_size=None,
                reasoning_effort=self.reasoning_effort,
                use_human_message=False,
            )
        output = str(result.get("output", ""))
        if self.return_kind is None:
            self._completed = True
            return output

        error: ValueError | json.JSONDecodeError | None = None
        for repair_attempt in range(3):
            try:
                value = _validate_action_json(output, self.return_kind, self.name)
                self._completed = True
                return value
            except (json.JSONDecodeError, ValueError) as parse_error:
                error = parse_error
            if repair_attempt == 2:
                break
            result = await self._invoke(
                remote,
                action_name=f"{self.name}_json_repair",
                prompt=(
                    "Your immediately preceding response did not satisfy the action's JSON "
                    "contract. Repair that response without doing new research. Preserve all "
                    "recoverable information and return only one complete valid JSON value with "
                    f"a top-level {self.return_kind}. Do not use Markdown fences or commentary."
                ),
                arguments={
                    "expected_top_level": self.return_kind,
                    "parser_error": str(error),
                    "repair_attempt": repair_attempt + 1,
                },
                tools_enabled=False,
                search_context_size=None,
                reasoning_effort="low",
                use_human_message=False,
            )
            output = str(result.get("output", ""))

        raise ValueError(
            f"action {self.name} returned invalid JSON: {error}"
        ) from error

    async def _invoke(
        self,
        remote: dict[str, Any],
        *,
        action_name: str | None = None,
        prompt: str | None = None,
        arguments: dict[str, Any] | None = None,
        tools_enabled: bool = True,
        search_context_size: str | None = None,
        reasoning_effort: str | None = None,
        use_human_message: bool = False,
    ) -> Any:
        return await _effect(
            "invoke_action",
            {
                "agent_instance_id": remote["agent_instance_id"],
                "action_name": action_name or self.name,
                "prompt": prompt or self.prompt,
                "arguments": self.arguments if arguments is None else arguments,
                "response_format": self.response_format,
                "tools_enabled": tools_enabled,
                "requested_tools": self.tools if tools_enabled else [],
                "web_search_context_size": (
                    self.search_context_size
                    if search_context_size is None
                    else search_context_size
                ),
                "reasoning_effort": (
                    self.reasoning_effort
                    if reasoning_effort is None
                    else reasoning_effort
                ),
                "human_request_id": (
                    self.human_request_id if use_human_message else None
                ),
                "human_message_argument": (
                    self.human_message_parameter if use_human_message else None
                ),
            },
        )


def _validate_action_json(output: str, return_kind: str, action_name: str) -> Any:
    parsed = _parse_action_json(output, return_kind)
    expected = {
        "object": dict,
        "array": list,
        "boolean": bool,
        "integer": int,
        "number": (int, float),
    }[return_kind]
    if not isinstance(parsed, expected) or (
        return_kind in {"integer", "number"} and isinstance(parsed, bool)
    ):
        raise ValueError(
            f"action {action_name} must return a JSON {return_kind}, "
            f"got {type(parsed).__name__}"
        )
    return parsed


def _parse_action_json(output: str, return_kind: str) -> Any:
    """Parse a typed action result with a bounded provider-compatibility fallback.

    Responses-compatible providers do not all enforce structured output. The
    fallback accepts only one complete fenced payload, or the first decodable
    object/array beginning at the matching JSON delimiter. Type validation
    remains the caller's responsibility and arbitrary primitive values are not
    mined from prose.
    """
    try:
        return json.loads(output)
    except json.JSONDecodeError as original_error:
        stripped = output.strip()
        if stripped.startswith("```") and stripped.endswith("```"):
            first_newline = stripped.find("\n")
            if first_newline != -1:
                fenced = stripped[first_newline + 1 : -3].strip()
                try:
                    return json.loads(fenced)
                except json.JSONDecodeError:
                    pass
        delimiter = {"object": "{", "array": "["}.get(return_kind)
        if delimiter is not None:
            decoder = json.JSONDecoder()
            for index, character in enumerate(output):
                if character != delimiter:
                    continue
                try:
                    value, _ = decoder.raw_decode(output, index)
                except json.JSONDecodeError:
                    continue
                return value
        raise original_error


async def together(*actions: Awaitable[T]) -> tuple[T, ...]:
    explicit = [item for item in actions if isinstance(item, _ActionCall)]
    keys = [id(item.agent) for item in explicit]
    if len(keys) != len(set(keys)):
        raise ValueError("together() cannot run two actions on the same Agent Session")
    fork_path = _reserve_effect_path("together")
    branches = [
        _run_in_effect_branch(action, (*fork_path, f"branch:{index}"))
        for index, action in enumerate(actions)
    ]
    return tuple(await asyncio.gather(*branches))


async def ask_human(
    question: str,
    *,
    response_schema: dict[str, Any] | None = None,
    agent: Agent | None = None,
) -> Any:
    agent_id = None
    if agent is not None:
        agent_id = (await agent._ensure_remote())["agent_instance_id"]
    result = await _effect(
        "ask_human",
        {
            "question": question,
            "response_schema": response_schema or {"type": "string"},
            "agent_instance_id": agent_id,
        },
    )
    answer = result["answer"]
    if isinstance(answer, str):
        return HumanMessage(answer, str(result["human_request_id"]))
    return answer


@dataclass(frozen=True)
class ArtifactRef:
    id: str
    name: str
    kind: str
    media_type: str
    size_bytes: int


async def publish_artifact(
    name: str,
    content: str,
    *,
    kind: str = "other",
    media_type: str = "text/plain; charset=utf-8",
    metadata: dict[str, Any] | None = None,
    agent: Agent | None = None,
) -> ArtifactRef:
    if not isinstance(content, str):
        raise TypeError("publish_artifact() content must be text")
    agent_id = None
    if agent is not None:
        agent_id = (await agent._ensure_remote())["agent_instance_id"]
    result = await _effect(
        "publish_artifact",
        {
            "name": name,
            "content": content,
            "kind": kind,
            "media_type": media_type,
            "metadata": metadata or {},
            "agent_instance_id": agent_id,
        },
    )
    return ArtifactRef(
        id=str(result["artifact_id"]),
        name=str(result["name"]),
        kind=str(result["kind"]),
        media_type=str(result["media_type"]),
        size_bytes=int(result["size_bytes"]),
    )


async def publish_project_home(
    *,
    action: _ActionCall,
    metadata: dict[str, Any] | None = None,
) -> ArtifactRef:
    """Publish the HTML returned by one completed Action call as Project home."""

    if not isinstance(action, _ActionCall):
        raise TypeError("publish_project_home action must be an Action call")
    if not action._completed or action.action_invocation_id is None:
        raise RuntimeError("publish_project_home action must be awaited successfully first")
    result = await _effect(
        "publish_project_home",
        {
            "action_invocation_id": action.action_invocation_id,
            "metadata": metadata or {},
        },
    )
    return ArtifactRef(
        id=str(result["artifact_id"]),
        name=str(result["name"]),
        kind=str(result["kind"]),
        media_type=str(result["media_type"]),
        size_bytes=int(result["size_bytes"]),
    )


async def wait(
    *,
    seconds: float | None = None,
    minutes: float | None = None,
    name: str = "wait",
) -> dict[str, Any]:
    interval = seconds if seconds is not None else (minutes or 0) * 60
    if interval <= 0:
        raise ValueError("wait() requires a positive seconds or minutes interval")
    return await _effect(
        "wait",
        {
            "name": name,
            "interval_ms": max(1, int(interval * 1000)),
        },
    )


class ProjectContext:
    async def changes(
        self,
        *,
        after_cursor: int | None = None,
    ) -> dict[str, Any]:
        return await _effect(
            "project_changes",
            {"after_cursor": after_cursor},
        )


@dataclass(frozen=True)
class WorkflowContext:
    request: str
    instructions: str
    params: dict[str, Any]
    workflow_id: str
    trigger: dict[str, Any] = field(default_factory=dict)

    @property
    def project(self) -> ProjectContext:
        return ProjectContext()


class _Runtime:
    def __init__(
        self,
        send: Callable[[str, str, dict[str, Any]], Awaitable[Any]],
    ) -> None:
        self.send = send


def _set_runtime(runtime: _Runtime) -> None:
    global _runtime
    _runtime = runtime
    _effect_cursor.set(_EffectCursor(("root",)))


def _require_runtime() -> _Runtime:
    if _runtime is None:
        raise RuntimeError("PaperMachine DSL operation used outside a Workflow")
    return _runtime


def _normalize_action_tools(values: list[str]) -> list[str]:
    if not isinstance(values, list) or any(
        not isinstance(value, str) or not value.strip() for value in values
    ):
        raise ValueError("action tools must be a list of non-empty names")
    normalized = [value.strip() for value in values]
    if len(normalized) != len(set(normalized)):
        raise ValueError("action tools must not contain duplicates")
    return normalized


async def _effect(
    kind: str,
    payload: dict[str, Any],
    *,
    effect_id: str | None = None,
) -> Any:
    json.dumps(payload)
    if effect_id is None:
        effect_id = _effect_id(_reserve_effect_path("effect"), kind)
    return await _require_runtime().send(effect_id, kind, payload)


__all__ = [
    "Agent",
    "ArtifactRef",
    "HumanMessage",
    "ProjectContext",
    "WorkflowContext",
    "action",
    "ask_human",
    "publish_artifact",
    "publish_project_home",
    "together",
    "wait",
    "workflow",
]
