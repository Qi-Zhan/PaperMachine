from __future__ import annotations

import ast
import json
import re
import sys
import uuid
from typing import Any

MAX_SOURCE_BYTES = 128 * 1024
ALLOWED_IMPORTS = {"papermachine"}
FORBIDDEN_CALLS = {
    "breakpoint",
    "compile",
    "delattr",
    "eval",
    "exec",
    "getattr",
    "globals",
    "help",
    "input",
    "locals",
    "open",
    "setattr",
    "vars",
    "__import__",
}
FORBIDDEN_ROOTS = {
    "builtins",
    "ctypes",
    "importlib",
    "io",
    "multiprocessing",
    "os",
    "pathlib",
    "pickle",
    "shutil",
    "signal",
    "socket",
    "subprocess",
    "sys",
    "threading",
}
SLUG = re.compile(r"^[a-z0-9]+(?:-[a-z0-9]+)*$")
ACCESS_PROFILES = {
    "model_only",
    "read_only",
    "workspace",
    "research",
    "full_access",
}
WORKFLOW_METADATA_KEYS = {
    "slug",
    "name",
    "description",
    "request_mode",
    "params_schema",
    "output_schema",
    "entrypoint",
}


class Validator(ast.NodeVisitor):
    def __init__(self) -> None:
        self.diagnostics: list[dict[str, Any]] = []
        self.agents: list[dict[str, Any]] = []
        self.features: dict[str, Any] = {
            "parallel_blocks": 0,
            "teams": [],
            "relations": 0,
            "scopes": [],
            "channels": [],
            "timers": [],
            "human_checkpoints": 0,
            "background_tasks": 0,
            "project_snapshots": 0,
            "artifacts": 0,
        }

    def error(self, node: ast.AST, message: str) -> None:
        self.diagnostics.append(
            {
                "severity": "error",
                "message": message,
                "line": getattr(node, "lineno", None),
                "column": getattr(node, "col_offset", None),
            }
        )

    def visit_Import(self, node: ast.Import) -> None:
        self.error(node, "use only `from papermachine import ...`; arbitrary imports are disabled")

    def visit_ImportFrom(self, node: ast.ImportFrom) -> None:
        if node.level != 0 or node.module not in ALLOWED_IMPORTS:
            self.error(node, "workflow source may import only from papermachine")

    def visit_Name(self, node: ast.Name) -> None:
        if node.id in FORBIDDEN_ROOTS or node.id.startswith("__"):
            self.error(node, f"name `{node.id}` is not available in workflow source")

    def visit_Attribute(self, node: ast.Attribute) -> None:
        if node.attr.startswith("__"):
            self.error(node, "dunder attribute access is disabled")
        root = node.value
        while isinstance(root, ast.Attribute):
            root = root.value
        if isinstance(root, ast.Name) and root.id in FORBIDDEN_ROOTS:
            self.error(node, f"access through `{root.id}` is disabled")
        self.generic_visit(node)

    def visit_Call(self, node: ast.Call) -> None:
        if isinstance(node.func, ast.Name) and node.func.id in FORBIDDEN_CALLS:
            self.error(node, f"call to `{node.func.id}` is disabled")
        if isinstance(node.func, ast.Name):
            name = node.func.id
            if name == "together":
                self.features["parallel_blocks"] += 1
            elif name == "Team":
                self.features["teams"].append(literal_string_arg(node))
            elif name == "relate":
                self.features["relations"] += 1
            elif name == "scope":
                self.features["scopes"].append(literal_string_arg(node))
            elif name == "Channel":
                self.features["channels"].append(literal_string_arg(node))
            elif name == "ask_human":
                self.features["human_checkpoints"] += 1
            elif name == "background":
                self.features["background_tasks"] += 1
            elif name == "publish_artifact":
                self.features["artifacts"] += 1
            elif name == "wait":
                values = literal_call_keywords(node)
                seconds = values.get("seconds")
                minutes = values.get("minutes")
                interval = seconds if isinstance(seconds, (int, float)) else None
                if interval is None and isinstance(minutes, (int, float)):
                    interval = minutes * 60
                self.features["timers"].append(
                    {
                        "callback": values.get("name", "wait"),
                        "seconds": float(interval) if interval is not None else None,
                        "policy": values.get("policy"),
                    }
                )
        elif isinstance(node.func, ast.Attribute) and node.func.attr == "snapshot":
            self.features["project_snapshots"] += 1
        self.generic_visit(node)

    def visit_AsyncFunctionDef(self, node: ast.AsyncFunctionDef) -> None:
        for decorator in node.decorator_list:
            if not (isinstance(decorator, ast.Call) and isinstance(decorator.func, ast.Name)):
                continue
            if decorator.func.id != "every":
                continue
            values = literal_call_keywords(decorator)
            seconds = values.get("seconds")
            policy = values.get("policy")
            self.features["timers"].append(
                {
                    "callback": node.name,
                    "seconds": float(seconds) if isinstance(seconds, (int, float)) else None,
                    "policy": policy if isinstance(policy, str) else None,
                }
            )
        self.generic_visit(node)

    def visit_ClassDef(self, node: ast.ClassDef) -> None:
        if any(isinstance(base, ast.Name) and base.id == "Agent" for base in node.bases):
            actions = []
            access = "research"
            for item in node.body:
                if isinstance(item, (ast.FunctionDef, ast.AsyncFunctionDef)) and has_decorator(item, "action"):
                    actions.append(item.name)
                if not (
                    isinstance(item, (ast.Assign, ast.AnnAssign))
                    and assignment_name(item) == "access"
                ):
                    continue
                value = item.value
                if not isinstance(value, ast.Constant) or not isinstance(value.value, str):
                    self.error(item, "Agent access must be a literal profile name")
                elif value.value not in ACCESS_PROFILES:
                    expected = ", ".join(sorted(ACCESS_PROFILES))
                    self.error(item, f"Agent access must be one of: {expected}")
                else:
                    access = value.value
            self.agents.append({"class_name": node.name, "actions": actions, "access": access})
        self.generic_visit(node)


def has_decorator(node: ast.AST, name: str) -> bool:
    for decorator in getattr(node, "decorator_list", []):
        target = decorator.func if isinstance(decorator, ast.Call) else decorator
        if isinstance(target, ast.Name) and target.id == name:
            return True
    return False


def assignment_name(node: ast.Assign | ast.AnnAssign) -> str | None:
    if isinstance(node, ast.AnnAssign):
        return node.target.id if isinstance(node.target, ast.Name) else None
    if len(node.targets) != 1 or not isinstance(node.targets[0], ast.Name):
        return None
    return node.targets[0].id


def literal_string_arg(node: ast.Call) -> str:
    if node.args and isinstance(node.args[0], ast.Constant) and isinstance(node.args[0].value, str):
        return node.args[0].value
    return "dynamic"


def literal_call_keywords(node: ast.Call) -> dict[str, Any]:
    values: dict[str, Any] = {}
    for keyword in node.keywords:
        if keyword.arg is None:
            continue
        try:
            values[keyword.arg] = ast.literal_eval(keyword.value)
        except (ValueError, TypeError, SyntaxError):
            continue
    return values


def literal_keywords(decorator: ast.Call) -> dict[str, Any]:
    values: dict[str, Any] = {}
    for keyword in decorator.keywords:
        if keyword.arg is None:
            raise ValueError("workflow metadata does not accept ** expansion")
        values[keyword.arg] = ast.literal_eval(keyword.value)
    return values


def find_manifest(tree: ast.Module) -> tuple[dict[str, Any] | None, ast.AST | None, str | None]:
    found: list[tuple[dict[str, Any], ast.AST, str]] = []
    for node in tree.body:
        if not isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
            continue
        for decorator in node.decorator_list:
            if isinstance(decorator, ast.Call) and isinstance(decorator.func, ast.Name) and decorator.func.id == "workflow":
                found.append((literal_keywords(decorator), decorator, node.name))
    if len(found) != 1:
        return None, tree, None
    metadata, node, entrypoint = found[0]
    metadata["entrypoint"] = entrypoint
    return metadata, node, entrypoint


def validate(source: str) -> dict[str, Any]:
    diagnostics: list[dict[str, Any]] = []
    if len(source.encode("utf-8")) > MAX_SOURCE_BYTES:
        diagnostics.append({"severity": "error", "message": "workflow source exceeds 128 KiB", "line": None, "column": None})
        return {"valid": False, "manifest": None, "agents": [], "features": Validator().features, "diagnostics": diagnostics}
    try:
        tree = ast.parse(source, filename="workflow.py")
    except SyntaxError as error:
        diagnostics.append(
            {
                "severity": "error",
                "message": error.msg,
                "line": error.lineno,
                "column": error.offset,
            }
        )
        return {"valid": False, "manifest": None, "agents": [], "features": Validator().features, "diagnostics": diagnostics}

    validator = Validator()
    validator.visit(tree)
    diagnostics.extend(validator.diagnostics)
    try:
        metadata, node, entrypoint = find_manifest(tree)
    except (ValueError, TypeError, SyntaxError) as error:
        metadata, node, entrypoint = None, tree, None
        diagnostics.append({"severity": "error", "message": f"workflow metadata must contain literal values: {error}", "line": None, "column": None})
    if metadata is None:
        diagnostics.append({"severity": "error", "message": "source must define exactly one function decorated with @workflow(...) ", "line": getattr(node, "lineno", None), "column": getattr(node, "col_offset", None)})
        return {"valid": False, "manifest": None, "agents": validator.agents, "features": validator.features, "diagnostics": diagnostics}

    slug = str(metadata.get("slug", ""))
    name = str(metadata.get("name", ""))
    description = str(metadata.get("description", ""))
    request_mode = str(metadata.get("request_mode", "required"))
    unknown_metadata = sorted(set(metadata) - WORKFLOW_METADATA_KEYS)
    if unknown_metadata:
        diagnostics.append(
            {
                "severity": "error",
                "message": "unknown workflow metadata: "
                + ", ".join(unknown_metadata),
                "line": getattr(node, "lineno", None),
                "column": getattr(node, "col_offset", None),
            }
        )
    for condition, message in [
        (not SLUG.fullmatch(slug), "workflow slug must use lowercase kebab-case"),
        (not name.strip(), "workflow name is required"),
        (not description.strip(), "workflow description is required"),
        (request_mode not in {"required", "none"}, "request_mode must be required or none"),
        (not isinstance(metadata.get("params_schema", {}), dict), "params_schema must be a literal dict"),
        (not isinstance(metadata.get("output_schema", {}), dict), "output_schema must be a literal dict"),
    ]:
        if condition:
            diagnostics.append({"severity": "error", "message": message, "line": getattr(node, "lineno", None), "column": getattr(node, "col_offset", None)})

    manifest = {
        "id": str(uuid.uuid5(uuid.NAMESPACE_URL, f"papermachine:workflow:{slug}")),
        "slug": slug,
        "name": name,
        "description": description,
        "entrypoint": entrypoint,
        "request_mode": request_mode,
        "params_schema": metadata.get("params_schema", {"type": "object"}),
        "output_schema": metadata.get("output_schema", {}),
    }
    return {
        "valid": not any(item["severity"] == "error" for item in diagnostics),
        "manifest": manifest,
        "agents": validator.agents,
        "features": validator.features,
        "diagnostics": diagnostics,
    }


def main() -> None:
    source = sys.stdin.read()
    print(json.dumps(validate(source), separators=(",", ":")))


if __name__ == "__main__":
    main()
