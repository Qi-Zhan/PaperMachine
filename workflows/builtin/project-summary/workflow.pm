version 1;

agent ProjectSummaryAgent {
    access = workspace;
    role = "Project curator";
    system = """
Maintain the Project home as the best current view of this research world.
Inspect the supplied current evidence and collaborate with relevant Agents when
useful. Return one complete standalone, script-free HTML document.
""";

    action maintain_project_home(changed_resources) {
        tools = [];
        search_context = low;
        prompt = "Maintain the Project home from the supplied changed resources.";
    }
}

workflow project_summary {
    slug = "project-summary";
    name = "Project summary";
    description = "Generate or periodically refresh the Project home page.";
    request = required;

    params {
        interval_minutes = number(default = 60, min = 0, title = "Refresh interval (minutes)", description = "Use 0 for a one-time update.");
    }

    run(ctx) {
        let summarizer = ProjectSummaryAgent(key = "main", name = "Project summary");
        var refresh_count = 0;
        var artifact_id = "";
        var cursor = null;

        loop {
            let changes = await ctx.project.changes(
                after_cursor = cursor,
                exclude_current_program = true,
            );
            let next_cursor = changes.cursor;
            if changes.changed {
                let home = await summarizer.maintain_project_home(
                    changed_resources = changes.resources,
                );
                let next_refresh_count = refresh_count + 1;
                let artifact = await publish_home(
                    action = home,
                    metadata = {
                        project_cursor: next_cursor,
                        refresh_count: next_refresh_count,
                        scheduled: ctx.params.interval_minutes > 0,
                    },
                );
                refresh_count = next_refresh_count;
                artifact_id = artifact.artifact_id;
            }
            cursor = next_cursor;
            if changes.has_more {
                continue;
            }
            if ctx.params.interval_minutes <= 0 {
                return {artifact_id, refresh_count};
            }
            await wait(
                minutes = ctx.params.interval_minutes,
                name = "project-summary-refresh",
            );
        }
    }
}
