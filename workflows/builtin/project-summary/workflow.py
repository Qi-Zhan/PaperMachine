from papermachine import Agent, action, publish_project_home, wait, workflow


class ProjectSummaryAgent(Agent):
    access = "model_only"
    role = "project progress curator"
    system_prompt = """You maintain the Project home page.

Inspect the current Project, its evidence, and the existing home page. Update the page so that it is an accurate and useful map of the Project now.

Use the available Project-home tools to edit the page incrementally. Inspect the actual materialized result after editing, verify important claims against Project evidence, and fix any problems you find. You may make as many editing and inspection passes as needed.

Prioritize the Project's objective, current state, verified conclusions, important evidence and deliverables, unresolved contradictions, blockers, and concrete next actions. Remove stale, duplicated, unsupported, or operational detail.

Do not organize the page around Agents, Sessions, Workflows, Runs, or Artifacts. They may be referenced only when they provide useful evidence or provenance.

Clearly distinguish verified, tentative, contradicted, blocked, and unknown claims. Never invent evidence, results, decisions, or completion. Work in the Project's language.

Finish only when the preview is accurate, coherent, useful, and supported by the available Project evidence."""

    @action(
        tools=[
            "read_project_home",
            "patch_project_home",
            "preview_project_home",
        ]
    )
    async def maintain_project_home(self, project_changes: dict):
        """Maintain the Project home page using the supplied Project snapshot or delta. First read the existing page. Use stable block IDs and patch only what should change; absence from a delta does not invalidate earlier content. Preview the complete materialized page, inspect it for missing or unsupported claims, duplication, stale conclusions, confusing hierarchy, and malformed or empty sections, and keep editing until the result is sound. Do not return page markup in your final message; the page must be created through the tools."""


@workflow(
    slug="project-summary",
    name="Project summary",
    description="Generate or periodically refresh the Project home page.",
    params_schema={
        "type": "object",
        "properties": {
            "interval_minutes": {
                "type": "number",
                "title": "Refresh interval (minutes)",
                "minimum": 0,
                "default": 60,
                "description": "Use 0 for a one-time update.",
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
)
async def main(ctx):
    interval_minutes = float(ctx.params.get("interval_minutes", 60))
    max_sessions = int(ctx.params.get("max_sessions", 50))
    turns_per_session = int(ctx.params.get("turns_per_session", 12))
    max_artifacts = int(ctx.params.get("max_artifacts", 50))
    summarizer = ProjectSummaryAgent(name="Project summary")
    refresh_count = 0
    artifact_id = ""
    snapshot_cursor = None

    while True:
        snapshot = await ctx.project.snapshot(
            after_cursor=snapshot_cursor,
            max_sessions=max_sessions,
            max_turns_per_session=turns_per_session,
            max_artifacts=max_artifacts,
            include_artifact_content=True,
        )
        next_cursor = snapshot["cursor"]
        if snapshot_cursor is not None and not snapshot["changed"]:
            snapshot_cursor = next_cursor
            if snapshot["has_more"]:
                continue
            await wait(
                minutes=interval_minutes,
                name="project-summary-refresh",
            )
            continue
        action = summarizer.maintain_project_home(snapshot)
        await action
        next_refresh_count = refresh_count + 1
        artifact = await publish_project_home(
            action=action,
            metadata={
                "snapshot_cursor": snapshot["cursor"],
                "snapshot_mode": snapshot["mode"],
                "after_cursor": snapshot["after_cursor"],
                "refresh_count": next_refresh_count,
                "scheduled": interval_minutes > 0,
            },
        )
        refresh_count = next_refresh_count
        artifact_id = artifact.id
        snapshot_cursor = next_cursor
        if interval_minutes <= 0:
            return {"artifact_id": artifact_id, "refresh_count": refresh_count}
        if snapshot["has_more"]:
            continue
        await wait(
            minutes=interval_minutes,
            name="project-summary-refresh",
        )
