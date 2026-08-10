from papermachine import Agent, action, publish_project_home, wait, workflow


class ProjectSummaryAgent(Agent):
    access = "model_only"
    role = "Project curator"
    system_prompt = """Maintain the Project home as a concise, evidence-grounded map of this research world. Read the relevant Project resources, including the existing home when useful. Emphasize the objective, key results and decisions, their provenance, open questions, and next actions. Use headings, lists, tables, and links when they improve the page. Never invent facts. Your entire final response must be the complete safe semantic HTML fragment in the Project's language: no commentary, Markdown fence, or outer article element."""

    @action(tools=["read_resource"])
    async def maintain_project_home(self, changed_resources: list[dict]):
        """Read the changed resources and anything they point to, then return only the complete current Project home HTML fragment."""


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
        changes = await ctx.project.changes(after_cursor=cursor)
        next_cursor = changes["cursor"]
        if cursor is not None and not changes["changed"]:
            cursor = next_cursor
            if changes["has_more"]:
                continue
            await wait(
                minutes=interval_minutes,
                name="project-summary-refresh",
            )
            continue
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
        if interval_minutes <= 0:
            return {"artifact_id": artifact_id, "refresh_count": refresh_count}
        if changes["has_more"]:
            continue
        await wait(
            minutes=interval_minutes,
            name="project-summary-refresh",
        )
