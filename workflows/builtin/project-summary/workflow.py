from papermachine import Agent, action, publish_artifact, wait, workflow


class ProjectSummaryAgent(Agent):
    access = "model_only"
    role = "project progress curator"
    system_prompt = """Build an honest, useful project progress page from the supplied PaperMachine snapshot. Treat Session Turns, Workflow outputs, and Artifact metadata as evidence. Separate completed findings, work in progress, unresolved questions, and recommended next actions. Preserve important uncertainty and failures instead of smoothing them over. Use the Project's working language. Never claim access to material absent from the snapshot."""

    @action(max_steps=12, max_output_tokens=24000)
    async def render_progress_page(self, snapshot: dict):
        """Return one complete, self-contained HTML document for the Project overview. Use semantic HTML and an inline <style>; do not use scripts, external assets, remote fonts, Markdown fences, or invented citations. Include a compact headline/status area, current conclusions with their Session or Workflow provenance, active work, blockers/open questions, and concrete next actions. The page is embedded in a sandboxed iframe, so make it responsive and readable on white or light neutral backgrounds."""


@workflow(
    slug="project-summary",
    name="Project summary",
    description="Generate the Project Page progress report once or refresh it on a durable timer. The Workflow system prompt is the user's summary policy.",
    input_schema={
        "type": "object",
        "properties": {
            "interval_minutes": {
                "type": "number",
                "title": "Refresh interval (minutes)",
                "minimum": 0,
                "default": 60,
                "description": "Use 0 for one refresh; a positive value keeps this Workflow scheduled.",
            },
            "max_sessions": {
                "type": "integer",
                "title": "Sessions to inspect",
                "minimum": 1,
                "maximum": 200,
                "default": 50,
            },
            "turns_per_session": {
                "type": "integer",
                "title": "Recent Turns per Session",
                "minimum": 1,
                "maximum": 100,
                "default": 12,
            },
            "max_artifacts": {
                "type": "integer",
                "title": "Artifacts to inspect",
                "minimum": 1,
                "maximum": 200,
                "default": 50,
            },
        },
        "additionalProperties": False,
    },
    output_schema={
        "type": "object",
        "properties": {
            "artifact_id": {"type": "string"},
            "refresh_count": {"type": "integer"},
        },
        "required": ["artifact_id", "refresh_count"],
    },
    budget={
        "max_agents": 1,
        "max_concurrent_actions": 1,
        "max_action_steps": 100000,
        "max_total_tokens": None,
        "max_uncached_tokens": None,
        "max_hosted_search_calls": 0,
        "max_wall_time_seconds": None,
        "max_cost_usd": None,
    },
)
async def main(ctx):
    interval_minutes = float(ctx.input.get("interval_minutes", 60))
    max_sessions = int(ctx.input.get("max_sessions", 50))
    turns_per_session = int(ctx.input.get("turns_per_session", 12))
    max_artifacts = int(ctx.input.get("max_artifacts", 50))
    summarizer = ProjectSummaryAgent(name="Project summary")
    refresh_count = 0
    artifact_id = ""

    while True:
        snapshot = await ctx.project.snapshot(
            max_sessions=max_sessions,
            max_turns_per_session=turns_per_session,
            max_artifacts=max_artifacts,
            include_artifact_content=True,
        )
        html = _normalize_html(await summarizer.render_progress_page(snapshot))
        refresh_count += 1
        artifact = await publish_artifact(
            "project-progress.html",
            html,
            kind="report",
            media_type="text/html; charset=utf-8",
            metadata={
                "role": "project_summary",
                "captured_at": snapshot["captured_at"],
                "refresh_count": refresh_count,
                "scheduled": interval_minutes > 0,
            },
            agent=summarizer,
        )
        artifact_id = artifact.id
        if interval_minutes <= 0:
            return {"artifact_id": artifact_id, "refresh_count": refresh_count}
        await wait(
            minutes=interval_minutes,
            name="project-summary-refresh",
            policy="coalesce",
        )


def _normalize_html(value):
    text = str(value or "").strip()
    if text.startswith("```") and text.endswith("```"):
        text = text[3:-3].strip()
        if text.lower().startswith("html"):
            text = text[4:].lstrip()
    lowered = text.lower()
    if "<html" in lowered or "<!doctype html" in lowered:
        return text
    escaped = (
        text.replace("&", "&amp;")
        .replace("<", "&lt;")
        .replace(">", "&gt;")
        .replace('"', "&quot;")
    )
    return f"""<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<style>body{{margin:0;padding:32px;color:#252b28;background:#f7f8f7;font:15px/1.65 system-ui,sans-serif}}article{{max-width:900px;margin:auto;padding:28px;background:white;border:1px solid #dfe3e1;border-radius:12px}}pre{{white-space:pre-wrap;font:inherit}}</style></head>
<body><article><h1>Project progress</h1><pre>{escaped}</pre></article></body></html>"""
