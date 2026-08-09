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
    "entrypoint",
}


class Validator(ast.NodeVisitor):
    def __init__(self) -> None:
        self.diagnostics: list[dict[str, Any]] = []
        self.agents: list[dict[str, Any]] = []

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
        self.generic_visit(node)

    def visit_ClassDef(self, node: ast.ClassDef) -> None:
        if any(isinstance(base, ast.Name) and base.id == "Agent" for base in node.bases):
            actions: list[dict[str, Any]] = []
            access = "research"
            for item in node.body:
                if isinstance(item, (ast.FunctionDef, ast.AsyncFunctionDef)) and has_decorator(item, "action"):
                    actions.append(
                        {
                            "name": item.name,
                            "tools": self.action_tools(item),
                        }
                    )
                if (
                    isinstance(item, (ast.Assign, ast.AnnAssign))
                    and assignment_name(item) == "access"
                ):
                    value = item.value
                    if not isinstance(value, ast.Constant) or not isinstance(value.value, str):
                        self.error(item, "Agent access must be a literal profile name")
                    elif value.value not in ACCESS_PROFILES:
                        expected = ", ".join(sorted(ACCESS_PROFILES))
                        self.error(item, f"Agent access must be one of: {expected}")
                    else:
                        access = value.value
            self.agents.append(
                {
                    "class_name": node.name,
                    "actions": actions,
                    "access": access,
                }
            )
        self.generic_visit(node)

    def action_tools(
        self, node: ast.FunctionDef | ast.AsyncFunctionDef
    ) -> list[str]:
        decorator = next(
            (
                item
                for item in node.decorator_list
                if (
                    isinstance(item, ast.Name)
                    and item.id == "action"
                )
                or (
                    isinstance(item, ast.Call)
                    and isinstance(item.func, ast.Name)
                    and item.func.id == "action"
                )
            ),
            None,
        )
        if not isinstance(decorator, ast.Call):
            return []
        keywords = [item for item in decorator.keywords if item.arg == "tools"]
        if not keywords:
            return []
        if len(keywords) > 1:
            self.error(decorator, "action tools may be declared only once")
            return []
        try:
            candidate = ast.literal_eval(keywords[0].value)
        except (ValueError, TypeError, SyntaxError):
            self.error(keywords[0].value, "action tools must be a literal list of names")
            return []
        if not isinstance(candidate, list) or any(
            not isinstance(value, str) or not value.strip() for value in candidate
        ):
            self.error(keywords[0].value, "action tools must be a literal list of non-empty names")
            return []
        normalized = [value.strip() for value in candidate]
        if len(normalized) != len(set(normalized)):
            self.error(keywords[0].value, "action tools must not contain duplicates")
            return []
        return normalized


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
        return {"valid": False, "manifest": None, "agents": [], "diagnostics": diagnostics}
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
        return {"valid": False, "manifest": None, "agents": [], "diagnostics": diagnostics}

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
        return {"valid": False, "manifest": None, "agents": validator.agents, "diagnostics": diagnostics}

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
    }
    return {
        "valid": not any(item["severity"] == "error" for item in diagnostics),
        "manifest": manifest,
        "agents": validator.agents,
        "diagnostics": diagnostics,
    }


def main() -> None:
    source = sys.stdin.read()
    print(json.dumps(validate(source), separators=(",", ":")))


if __name__ == "__main__":
    main()
