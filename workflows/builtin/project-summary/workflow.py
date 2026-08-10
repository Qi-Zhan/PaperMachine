from papermachine import Agent, action, publish_project_home, wait, workflow


class ProjectSummaryAgent(Agent):
    access = "workspace"
    role = "Project curator"
    system_prompt = """Maintain the Project home as the best current view of this research world. Inspect the available evidence and collaborate with relevant Agents when useful. Return one complete standalone HTML document."""

    @action(search_context_size="low")
    async def maintain_project_home(self, changed_resources: list[dict]):
        """Maintain the Project home."""


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
        },
        "additionalProperties": False,
    },
)
async def main(ctx):
    interval_minutes = float(ctx.params.get("interval_minutes", 60))
    summarizer = ProjectSummaryAgent(name="Project summary")
    refresh_count = 0
    artifact_id = ""
    cursor = None

    while True:
        changes = await ctx.project.changes(
            after_cursor=cursor,
            exclude_current_program=True,
        )
        next_cursor = changes["cursor"]
        if changes["changed"]:
            action = summarizer.maintain_project_home(changes["resources"])
            await action
            next_refresh_count = refresh_count + 1
            artifact = await publish_project_home(
                action=action,
                metadata={
                    "project_cursor": next_cursor,
                    "refresh_count": next_refresh_count,
                    "scheduled": interval_minutes > 0,
                },
            )
            refresh_count = next_refresh_count
            artifact_id = artifact.id
        cursor = next_cursor
        if changes["has_more"]:
            continue
        if interval_minutes <= 0:
            return {"artifact_id": artifact_id, "refresh_count": refresh_count}
        await wait(
            minutes=interval_minutes,
            name="project-summary-refresh",
        )
