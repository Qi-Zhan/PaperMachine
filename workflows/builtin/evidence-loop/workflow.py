from papermachine import Agent, action, together, workflow


class Planner(Agent):
    access = "model_only"
    role = "research planner"
    system_prompt = """Plan the research needed to answer the user's actual request. Keep the requested deliverable and constraints intact. Choose independent routes that can find and challenge evidence rather than splitting one conjunction across different candidates. Do not answer the question yourself."""

    @action(reasoning_effort="medium", tools=[])
    async def plan(
        self,
        question: str,
        route_count: int,
        extra_requirements: list[str],
        feedback: str,
    ) -> dict:
        """Return a JSON object with deliverable, acceptance_criteria, routes, and verification_notes. routes must contain exactly route_count objects, each with a concise name and an independent objective. Every route receives the complete question. If feedback is non-empty, fix the structural problem it describes and return the full plan again."""


class Researcher(Agent):
    access = "model_only"
    role = "independent evidence route"
    system_prompt = """Research the full user request through your assigned route. Use the live tools that are actually available, search beyond the first plausible answer, and verify consequential claims with primary or authoritative sources. Never merge evidence about different candidates into one answer. Preserve direct URLs, exact names, values, dates, contradictions, uncertainty, and unresolved gaps. Return an evidence report, not a polished final answer."""

    @action(
        search_context_size="low",
        reasoning_effort="high",
        tools=[],
    )
    async def research(
        self,
        question: str,
        plan: dict,
        objective: str,
        phase: str,
    ):
        """Use tools to investigate objective while retaining the complete question and plan. For an initial phase, establish an independent evidence route. For a follow-up phase, continue from this same Session and investigate only the evaluator's remaining question. Report evidence with direct source URLs, counterevidence, confidence, and gaps."""


class Evaluator(Agent):
    access = "model_only"
    role = "evidence and draft evaluator"
    system_prompt = """Judge whether the accumulated evidence can support the user's requested deliverable. Check candidate identity, source quality, coverage, contradictions, uncertainty, and exact output constraints. Ask for narrow follow-ups on existing research routes when material gaps are still researchable. Do not replace missing evidence with your own guesses."""

    @action(reasoning_effort="high", tools=[])
    async def assess(
        self,
        question: str,
        plan: dict,
        evidence_ledger: list[dict],
        round_number: int,
        feedback: str,
    ) -> dict:
        """Return complete (boolean), rationale (string), supported_conclusions (array), unresolved_gaps (array), contradictions (array), and follow_ups (array). Each follow-up must contain route_index and objective, referring to an existing route. complete may be true only when no follow-up research is needed. If feedback is non-empty, correct the structural inconsistency and return the full assessment again."""

    @action(reasoning_effort="high", tools=[])
    async def review_draft(
        self,
        question: str,
        plan: dict,
        evaluation: dict,
        draft: str,
        review_number: int,
    ) -> dict:
        """Return complete (boolean) and feedback (string). Find unsupported claims, source or candidate mix-ups, missing requested parts, hidden uncertainty, and output-format errors. When incomplete, give concrete repair instructions; when complete, explain briefly why the draft is ready."""


class Writer(Agent):
    access = "model_only"
    role = "evidence-grounded writer"
    system_prompt = """Produce the user's requested deliverable from the plan, evidence ledger, and evaluator judgment. Cite direct URLs near factual claims, preserve uncertainty, never invent missing values, and obey structured-output requirements exactly."""

    @action(reasoning_effort="high", tools=[])
    async def compose(
        self,
        question: str,
        plan: dict,
        evidence_ledger: list[dict],
        evaluation: dict,
    ):
        """Return only the complete user-facing deliverable."""

    @action(reasoning_effort="medium", tools=[])
    async def revise(self, review: dict):
        """Revise the preceding draft using every concrete review instruction. Return the complete corrected deliverable without adding unsupported evidence."""


def _plan_error(plan, route_count):
    if not isinstance(plan, dict):
        return "plan must be a JSON object"
    if not str(plan.get("deliverable") or "").strip():
        return "deliverable must be a non-empty string"
    routes = plan.get("routes")
    if not isinstance(routes, list) or len(routes) != route_count:
        return f"routes must contain exactly {route_count} entries"
    for index, route in enumerate(routes):
        if not isinstance(route, dict):
            return f"routes[{index}] must be an object"
        if not str(route.get("name") or "").strip():
            return f"routes[{index}].name must be non-empty"
        if not str(route.get("objective") or "").strip():
            return f"routes[{index}].objective must be non-empty"
    return ""


async def _create_plan(
    planner,
    question,
    route_count,
    extra_requirements,
):
    feedback = ""
    for _attempt in range(2):
        plan = await planner.plan(
            question,
            route_count,
            extra_requirements,
            feedback,
        )
        error = _plan_error(plan, route_count)
        if not error:
            return plan
        feedback = error
    raise ValueError("Planner did not return a usable plan: " + feedback)


def _assessment_error(assessment, route_count):
    if not isinstance(assessment, dict):
        return "assessment must be a JSON object"
    if not isinstance(assessment.get("complete"), bool):
        return "complete must be a boolean"
    follow_ups = assessment.get("follow_ups")
    if not isinstance(follow_ups, list):
        return "follow_ups must be an array"
    if assessment["complete"] and follow_ups:
        return "a complete assessment cannot request follow-up research"
    for index, follow_up in enumerate(follow_ups):
        if not isinstance(follow_up, dict):
            return f"follow_ups[{index}] must be an object"
        route_index = follow_up.get("route_index")
        if not isinstance(route_index, int) or not 0 <= route_index < route_count:
            return f"follow_ups[{index}].route_index is outside the route list"
        if not str(follow_up.get("objective") or "").strip():
            return f"follow_ups[{index}].objective must be non-empty"
    return ""


async def _assess(
    evaluator,
    question,
    plan,
    evidence_ledger,
    round_number,
    route_count,
):
    feedback = ""
    for _attempt in range(2):
        assessment = await evaluator.assess(
            question,
            plan,
            evidence_ledger,
            round_number,
            feedback,
        )
        error = _assessment_error(assessment, route_count)
        if not error:
            return assessment
        feedback = error
    raise ValueError("Evaluator did not return a usable assessment: " + feedback)


def _follow_ups(assessment, limit):
    return list(assessment.get("follow_ups") or [])[:limit]


@workflow(
    slug="evidence-loop",
    name="Evidence loop",
    description="Research a question through independent evidence routes, evaluate gaps, and revise the final draft.",
    params_schema={
        "type": "object",
        "properties": {
            "route_count": {"type": "integer", "minimum": 2, "maximum": 4},
            "extra_requirements": {"type": "array", "items": {"type": "string"}},
            "max_rounds": {"type": "integer", "minimum": 1, "maximum": 4},
            "max_followups_per_round": {"type": "integer", "minimum": 1, "maximum": 4},
            "max_draft_revisions": {"type": "integer", "minimum": 0, "maximum": 3},
            "planner_model": {
                "type": "string",
                "format": "model-profile",
                "title": "Planner model",
            },
            "research_model": {
                "type": "string",
                "format": "model-profile",
                "title": "Research model",
            },
            "evaluator_model": {
                "type": "string",
                "format": "model-profile",
                "title": "Evaluator model",
            },
            "writer_model": {
                "type": "string",
                "format": "model-profile",
                "title": "Writer model",
            },
        },
        "additionalProperties": False,
    },
)
async def main(ctx):
    route_count = max(2, min(int(ctx.params.get("route_count", 2)), 4))
    max_rounds = max(1, min(int(ctx.params.get("max_rounds", 2)), 4))
    max_followups = max(
        1,
        min(int(ctx.params.get("max_followups_per_round", route_count)), route_count),
    )
    max_draft_revisions = max(
        0,
        min(int(ctx.params.get("max_draft_revisions", 2)), 3),
    )
    extra_requirements = [
        str(value).strip()
        for value in ctx.params.get("extra_requirements") or []
        if str(value).strip()
    ]
    planner_model = str(ctx.params.get("planner_model") or "")
    research_model = str(ctx.params.get("research_model") or "")
    evaluator_model = str(ctx.params.get("evaluator_model") or "")
    writer_model = str(ctx.params.get("writer_model") or "")

    planner = Planner(name="Planner", model=planner_model)
    evaluator = Evaluator(name="Evaluator", model=evaluator_model)
    writer = Writer(name="Writer", model=writer_model)

    plan = await _create_plan(
        planner,
        ctx.request,
        route_count,
        extra_requirements,
    )
    routes = plan["routes"]
    researchers = [
        Researcher(
            name=str(route["name"]),
            role=str(route["objective"]),
            model=research_model,
        )
        for route in routes
    ]
    reports = await together(
        *(
            researcher.research(
                ctx.request,
                plan,
                str(route["objective"]),
                "initial",
            )
            for researcher, route in zip(researchers, routes)
        )
    )
    ledger = [
        {
            "route_index": index,
            "route_name": str(routes[index]["name"]),
            "phase": "initial",
            "objective": str(routes[index]["objective"]),
            "report": report,
        }
        for index, report in enumerate(reports)
    ]

    round_number = 1
    reused_sessions = False
    evaluation = await _assess(
        evaluator,
        ctx.request,
        plan,
        ledger,
        round_number,
        route_count,
    )
    while not evaluation["complete"] and round_number < max_rounds:
        follow_ups = _follow_ups(evaluation, max_followups)
        if not follow_ups:
            break
        round_number += 1
        reused_sessions = True
        reports = await together(
            *(
                researchers[item["route_index"]].research(
                    ctx.request,
                    plan,
                    str(item["objective"]),
                    "follow_up",
                )
                for item in follow_ups
            )
        )
        ledger.extend(
            {
                "route_index": item["route_index"],
                "route_name": str(routes[item["route_index"]]["name"]),
                "phase": "follow_up",
                "objective": str(item["objective"]),
                "report": report,
            }
            for item, report in zip(follow_ups, reports)
        )
        evaluation = await _assess(
            evaluator,
            ctx.request,
            plan,
            ledger,
            round_number,
            route_count,
        )

    report = await writer.compose(ctx.request, plan, ledger, evaluation)
    reviews = []
    for review_number in range(max_draft_revisions + 1):
        review = await evaluator.review_draft(
            ctx.request,
            plan,
            evaluation,
            report,
            review_number + 1,
        )
        if not isinstance(review, dict) or not isinstance(review.get("complete"), bool):
            raise ValueError("Draft review must return a JSON object with complete boolean")
        reviews.append(review)
        if review["complete"] or review_number == max_draft_revisions:
            break
        report = await writer.revise(review)

    draft_complete = reviews[-1]["complete"]
    reasons = []
    if not evaluation["complete"]:
        reasons.append("evidence_incomplete")
    if not draft_complete:
        reasons.append("draft_review_incomplete")
    return {
        "report": report,
        "plan": plan,
        "evaluation": evaluation,
        "draft_audit": {
            "pass": draft_complete,
            "revision_performed": len(reviews) > 1,
            "initial": reviews[0],
            "final": reviews[-1],
            "attempts": reviews,
        },
        "rounds": round_number,
        "evidence_ledger": ledger,
        "route_sessions_reused": reused_sessions,
        "completion": {
            "status": "passed" if not reasons else "warning",
            "quality_gate_pass": not reasons,
            "reasons": reasons,
        },
    }
