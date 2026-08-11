version 1;

schema Route = object {
    key: string,
    name: string,
    objective: string,
};

schema ResearchPlan = object {
    deliverable: string,
    acceptance_criteria: list(string),
    routes: list(Route),
    verification_notes: list(string),
};

schema FollowUp = object {
    route_key: string,
    objective: string,
};

schema Assessment = object {
    complete: bool,
    rationale: string,
    supported_conclusions: list(string),
    unresolved_gaps: list(string),
    contradictions: list(string),
    follow_ups: list(FollowUp),
};

schema DraftReview = object {
    complete: bool,
    feedback: string,
};

agent Planner {
    access = model_only;
    role = "research planner";
    system = """
Plan the research needed to answer the complete request. Keep its deliverable
and constraints intact. Choose independent routes that can find and challenge
evidence rather than splitting one conjunction across candidates. Do not answer
the question yourself.
""";

    action plan(question, route_count, extra_requirements, feedback) {
        tools = [];
        reasoning_effort = medium;
        result = ResearchPlan;
        prompt = """
Return the complete research plan. Include exactly route_count independent
routes. Each route needs a unique stable scalar key, concise name, and objective;
every route receives the complete question. If feedback is non-empty, correct
that structural problem and return the whole plan again.
""";
    }
}

agent Researcher {
    access = model_only;
    role = "independent evidence route";
    system = """
Research the complete request through the assigned route. Use the live tools
actually available, search beyond the first plausible answer, and verify
consequential claims with primary or authoritative sources. Preserve direct
URLs, exact names, values, dates, contradictions, uncertainty, and gaps. Return
an evidence report rather than a polished final answer.
""";

    action research(question, plan, objective, phase) {
        tools = [];
        search_context = low;
        reasoning_effort = high;
        prompt = """
Investigate objective while retaining the complete question and plan. In an
initial phase establish an independent route. In a follow-up phase continue this
same Agent worldline and investigate only the evaluator's remaining question.
Return evidence with direct URLs, counterevidence, confidence, and gaps.
""";
    }
}

agent Evaluator {
    access = model_only;
    role = "evidence and draft evaluator";
    system = """
Judge whether accumulated evidence supports the requested deliverable. Check
candidate identity, source quality, coverage, contradictions, uncertainty, and
exact output constraints. Request narrow follow-ups on existing route keys when
material gaps remain. Never replace missing evidence with guesses.
""";

    action assess(question, plan, evidence_ledger, round_number, feedback) {
        tools = [];
        reasoning_effort = high;
        result = Assessment;
        prompt = """
Assess all evidence. A complete assessment cannot request follow-ups. Every
follow-up must name an existing route_key and one narrow objective. If feedback
is non-empty, correct the inconsistency and return the full assessment again.
""";
    }

    action review_draft(question, plan, evaluation, draft, review_number) {
        tools = [];
        reasoning_effort = high;
        result = DraftReview;
        prompt = """
Find unsupported claims, source or candidate mix-ups, missing requested parts,
hidden uncertainty, and output-format errors. If incomplete, give concrete
repair instructions; if complete, explain briefly why the draft is ready.
""";
    }
}

agent Writer {
    access = model_only;
    role = "evidence-grounded writer";
    system = """
Produce the requested deliverable only from the plan, evidence ledger, and
evaluator judgment. Cite direct URLs near factual claims, preserve uncertainty,
never invent missing values, and obey structured-output requirements exactly.
""";

    action compose(question, plan, evidence_ledger, evaluation) {
        tools = [];
        reasoning_effort = high;
        prompt = "Return only the complete user-facing deliverable.";
    }

    action revise(review) {
        tools = [];
        reasoning_effort = medium;
        prompt = "Revise the preceding draft using every concrete review instruction. Return the complete corrected deliverable without adding unsupported evidence.";
    }
}

fn create_plan(planner, question, route_count, extra_requirements) {
    var feedback = "";
    for attempt in range(2) {
        let plan = await planner.plan(
            question = question,
            route_count = route_count,
            extra_requirements = extra_requirements,
            feedback = feedback,
        );
        if len(plan.routes) == route_count {
            return plan;
        }
        feedback = "routes must contain exactly " + string(route_count) + " entries";
    }
    fail("Planner did not return a usable plan: " + feedback)
}

fn assess_evidence(evaluator, question, plan, ledger, round_number) {
    var feedback = "";
    for attempt in range(2) {
        let assessment = await evaluator.assess(
            question = question,
            plan = plan,
            evidence_ledger = ledger,
            round_number = round_number,
            feedback = feedback,
        );
        if !(assessment.complete && len(assessment.follow_ups) > 0) {
            return assessment;
        }
        feedback = "a complete assessment cannot request follow-up research";
    }
    fail("Evaluator did not return a usable assessment: " + feedback)
}

workflow evidence_loop {
    slug = "evidence-loop";
    name = "Evidence loop";
    description = "Research through independent evidence routes, evaluate gaps, and revise the final draft.";
    request = required;

    params {
        route_count = int(default = 2, min = 2, max = 4);
        extra_requirements = list(string, default = []);
        max_rounds = int(default = 2, min = 1, max = 4);
        max_followups_per_round = int(default = 2, min = 1, max = 4);
        max_draft_revisions = int(default = 2, min = 0, max = 3);
        planner_model = model_profile(title = "Planner model");
        research_model = model_profile(title = "Research model");
        evaluator_model = model_profile(title = "Evaluator model");
        writer_model = model_profile(title = "Writer model");
    }

    run(ctx) {
        let route_count = clamp(get(ctx.params, "route_count", 2), 2, 4);
        let max_rounds = clamp(get(ctx.params, "max_rounds", 2), 1, 4);
        let max_followups = clamp(get(ctx.params, "max_followups_per_round", route_count), 1, route_count);
        let max_revisions = clamp(get(ctx.params, "max_draft_revisions", 2), 0, 3);
        let planner_model = get(ctx.params, "planner_model", "");
        let research_model = get(ctx.params, "research_model", "");
        let evaluator_model = get(ctx.params, "evaluator_model", "");
        let writer_model = get(ctx.params, "writer_model", "");
        let planner = Planner(key = "main", name = "Planner", model = planner_model);
        let evaluator = Evaluator(key = "main", name = "Evaluator", model = evaluator_model);
        let writer = Writer(key = "main", name = "Writer", model = writer_model);

        let plan = await create_plan(
            planner = planner,
            question = ctx.request,
            route_count = route_count,
            extra_requirements = get(ctx.params, "extra_requirements", []),
        );
        var ledger = parallel for route in plan.routes key route.key {
            let researcher = Researcher(
                key = route.key,
                name = route.key,
                model = research_model,
            );
            let report = await researcher.research(
                question = ctx.request,
                plan = plan,
                objective = route.objective,
                phase = "initial",
            );
            {
                route_key: route.key,
                route_name: route.name,
                phase: "initial",
                objective: route.objective,
                report,
            }
        };

        var round_number = 1;
        var reused_sessions = false;
        var evaluation = await assess_evidence(
            evaluator = evaluator,
            question = ctx.request,
            plan = plan,
            ledger = ledger,
            round_number = round_number,
        );

        while !evaluation.complete && round_number < max_rounds {
            let follow_ups = slice(evaluation.follow_ups, 0, max_followups);
            if len(follow_ups) == 0 {
                break;
            }
            round_number += 1;
            reused_sessions = true;
            let reports = parallel for item in follow_ups key item.route_key {
                let researcher = Researcher(
                    key = item.route_key,
                    name = item.route_key,
                    model = research_model,
                );
                let report = await researcher.research(
                    question = ctx.request,
                    plan = plan,
                    objective = item.objective,
                    phase = "follow_up",
                );
                {
                    route_key: item.route_key,
                    route_name: item.route_key,
                    phase: "follow_up",
                    objective: item.objective,
                    report,
                }
            };
            ledger = extend(ledger, reports);
            evaluation = await assess_evidence(
                evaluator = evaluator,
                question = ctx.request,
                plan = plan,
                ledger = ledger,
                round_number = round_number,
            );
        }

        var report = await writer.compose(
            question = ctx.request,
            plan = plan,
            evidence_ledger = ledger,
            evaluation = evaluation,
        );
        var reviews = [];
        var latest_review = {complete: false, feedback: "No review was run."};
        for review_index in range(max_revisions + 1) {
            let review = await evaluator.review_draft(
                question = ctx.request,
                plan = plan,
                evaluation = evaluation,
                draft = report,
                review_number = review_index + 1,
            );
            reviews = append(reviews, review);
            latest_review = review;
            if review.complete || review_index == max_revisions {
                break;
            }
            report = await writer.revise(review = review);
        }

        var reasons = [];
        if !evaluation.complete {
            reasons = append(reasons, "evidence_incomplete");
        }
        if !latest_review.complete {
            reasons = append(reasons, "draft_review_incomplete");
        }
        var completion_status = "warning";
        if len(reasons) == 0 {
            completion_status = "passed";
        }
        return {
            report,
            plan,
            evaluation,
            draft_audit: {
                pass: latest_review.complete,
                revision_performed: len(reviews) > 1,
                initial: reviews[0],
                final: latest_review,
                attempts: reviews,
            },
            rounds: round_number,
            evidence_ledger: ledger,
            route_sessions_reused: reused_sessions,
            completion: {
                status: completion_status,
                quality_gate_pass: len(reasons) == 0,
                reasons,
            },
        };
    }
}
