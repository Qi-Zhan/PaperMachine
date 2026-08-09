from papermachine import (
    Agent,
    HumanMessage,
    Team,
    action,
    ask_human,
    relate,
    scope,
    together,
    workflow,
)


DEFAULT_COVERAGE = [
    {
        "id": "answer",
        "requirement": "Answer every explicit part of the user's question.",
        "acceptance_test": "The final deliverable directly resolves the requested task.",
    },
    {
        "id": "evidence",
        "requirement": "Support material factual claims with direct authoritative sources.",
        "acceptance_test": "Important claims have source URLs and enough quoted or paraphrased evidence to verify them.",
    },
    {
        "id": "limits",
        "requirement": "Expose contradictions, uncertainty, scope limits, and unresolved gaps.",
        "acceptance_test": "The answer does not turn incomplete evidence into false certainty.",
    },
]


class Planner(Agent):
    access = "model_only"
    role = "research coverage planner"
    system_prompt = """You are only a research-coverage Planner. Never research the question, guess its answer, emit the user's requested final-answer format, or behave like the Writer. Turn the user's request into an explicit, question-specific coverage contract before research starts. Derive it only from the request, never from imagined hidden benchmark criteria or from the apparent domain of one named clue. First classify the request as exact_match (one identity must satisfy conjunctive clues), qualifying_list (every main-list candidate must satisfy an explicit qualification), option_survey (different strategies/tools may cover different subproblems and trade-offs), or explanatory_report (coverage applies to the report rather than candidate selection). Make requirements atomic and testable, but separately record which requirements truly must hold jointly for the same candidate, entity, source, row, or time period. Never turn an option survey into an all-or-nothing candidate filter. Preserve only output schemas, date cutoffs, exclusions, and exhaustiveness requirements explicitly present in the original question. Create genuinely independent search strategies rather than assigning one jointly-required property to each route. Every route receives the original question and full contract, and must be able to discover complete evidence on its own. Use three or four routes only when their source families, query strategies, or search spaces truly differ. Do not create a join, synthesis, completeness-audit, or final-formatting route; the evaluator and writer already perform those jobs."""

    @action(reasoning_effort="medium", tools=[])
    async def plan(
        self,
        question: str,
        minimum_route_count: int,
        maximum_route_count: int,
        extra_requirements: list[str],
        prior_context_brief: str,
        recovery_instruction: str,
    ) -> dict:
        """Return only the Planner JSON object; do not answer the research question. Required fields are answer_mode (exact_match, qualifying_list, option_survey, or explanatory_report), deliverable (string), output_contract (string), candidate_key (string describing what identifies one candidate when applicable), coverage_items (array of objects with id, requirement, acceptance_test), joint_constraints (array of strings describing only requirements that truly must be satisfied by the same candidate rather than by a portfolio of options), routes (array of objects with name, objective, coverage_ids), and verification_rules (array of strings). Create between minimum_route_count and maximum_route_count genuinely independent search routes, and produce no more than 16 coverage items. prior_context_brief contains potentially relevant earlier Project work; use it to avoid redundant routes and identify claims that need verification, but do not treat it as authoritative evidence. If recovery_instruction is non-empty, correct the prior contract violation it describes."""


class ContextAnalyst(Agent):
    access = "model_only"
    role = "prior Project context analyst"
    system_prompt = """Extract only prior Project material relevant to the new request. Treat every prior conclusion as an unverified lead unless its source and provenance are present. Preserve Session, Workflow, and Artifact identifiers; expose contradictions, missing sources, and work that should not be repeated. Do not answer the new request and do not invent evidence."""

    @action(reasoning_effort="medium", tools=[])
    async def distill(self, question: str, project_context: dict):
        """Return a compact brief with relevant prior findings, source leads, contradictions, unresolved gaps, reusable artifacts, and work to avoid repeating. Keep only material that can change the research plan for this question."""


class Researcher(Agent):
    access = "research"
    role = "evidence research route"
    system_prompt = """Use the live research tools actually exposed by the selected provider and access profile actively and persistently. When hosted web search is unavailable, do not pretend URL fetching is general search. Keep the original question and every joint constraint visible throughout the route: never turn a conjunction into independent facts and then combine different entities or papers into a synthetic match. Start with high-information exact clue combinations, then broaden deliberately and test alternative interpretations without silently inventing a domain. Reformulate queries when search is available, open promising pages, and verify important claims against primary or authoritative sources. Do not stop at the first plausible answer. Preserve exact names, numbers, dates, qualifications, and requested JSON fields. Return a candidate-centred evidence ledger rather than a prose report. Give every finding a stable candidate_id as well as coverage IDs; candidate_outputs must state which one candidate they describe and which joint constraints remain unresolved. Each material finding must include a direct source URL, a compact evidence excerpt or precise paraphrase, source type, confidence, and whether it is supported, contradicted, or unresolved. Include all genuinely qualifying items when the request asks for a comprehensive list, but keep weak or partial candidates out of candidate_outputs."""

    @action(
        search_context_size="low",
        reasoning_effort="high",
        tools=["read_file", "write_file", "exec_command", "fetch_url"],
    )
    async def research(
        self,
        question: str,
        objective: str,
        full_coverage_contract: list[dict],
        assigned_coverage_ids: list[str],
        joint_constraints: list[str],
        verification_rules: list[str],
        prior_context_brief: str,
        phase: str,
    ) -> dict:
        """Research the original question through the supplied search objective. Apply the full coverage contract and all joint constraints to every candidate; assigned_coverage_ids indicate this route's emphasis, not permission to ignore the remaining selection constraints. prior_context_brief contains unverified leads from earlier Project work: reuse useful query and source leads, avoid completed dead ends, but independently verify every material claim before adding it to findings. For phase=initial, establish an independent discovery route. For phase=evaluator_follow_up, continue from this same Session's existing evidence and investigate only the narrow unresolved objective without repeating completed searches. Return a JSON object with route_name, findings, candidate_outputs, contradictions, gaps, and searched_queries. findings must contain candidate_id, coverage_ids, claim, evidence, source_url, source_title, source_type, confidence, and status. A follow-up returns only a delta packet rather than restating prior evidence."""


class Evaluator(Agent):
    access = "model_only"
    role = "coverage and evidence evaluator"
    system_prompt = """Judge the evidence ledger against the frozen coverage contract, not against writing style. Honor answer_mode: exact_match and qualifying_list apply true joint constraints per candidate; option_survey applies coverage to the answer portfolio and retains evidence-backed options that solve distinct subproblems, with their scope and missing capabilities explicit; explanatory_report applies coverage to the report as a whole. First resolve candidate identity across routes. Evidence from several sources may corroborate one stable candidate, but evidence about different entities, materials, papers, versions, rows, or time periods must never be spliced into one candidate. Prefer precision over a long union of weakly related candidates, but do not reject a useful option merely because it does not independently solve every part of an option survey. Detect missing fields, unsupported or stale claims, incomplete supposedly exhaustive lists, contradictions, weak sources, and failure to obey the requested output format. Follow-ups must be narrow, evidence-seeking tasks assigned to an existing route index so that the same persistent Researcher Session can continue its work."""

    @action(reasoning_effort="high", tools=[])
    async def assess(
        self,
        question: str,
        plan: dict,
        evidence_ledger: list[dict],
        round_number: int,
    ) -> dict:
        """Return a JSON object with pass (boolean), score (0-100), rationale (string), coverage (array with coverage_id, status, evidence_refs, and gap), candidate_decisions (array with candidate_id, decision=approved/rejected/unresolved, satisfied_coverage_ids, failed_joint_constraints, evidence_refs, and reason), approved_candidates (array containing only output-ready candidates), contradictions (array), follow_ups (array with route_index, objective, and coverage_ids), and needs_human (boolean). Set pass=true only when every required coverage item has adequate evidence at the scope defined by answer_mode. Never approve a synthetic candidate assembled from different identities. For option_survey, an output-ready partial option is allowed when its actual role, scope, prerequisites, and gaps are explicit; for exact_match or qualifying_list, do not approve a merely topical or partial match unless the question requests it."""

    @action(reasoning_effort="high", tools=[])
    async def audit_draft(
        self,
        question: str,
        plan: dict,
        evaluation: dict,
        draft: str,
    ) -> dict:
        """Audit the writer's draft using the evidence ledger already present in this persistent evaluator Session. Return pass (boolean), format_errors (array), unsupported_outputs (array), omitted_approved_candidates (array), precision_errors (array), and repair_instructions (array). Reject malformed output, synthetic provenance, unapproved candidates, weak topical additions, and violations of joint constraints or the exact output contract. Set pass=false whenever any error array is non-empty; repairable errors still fail this audit until the revised draft is checked again."""


class Writer(Agent):
    access = "model_only"
    role = "evidence-grounded finalizer"
    system_prompt = """Produce the exact deliverable requested by the user from the frozen coverage contract and evidence ledger. Treat the evaluator's approved_candidates as an allow-list: do not dump the union of route candidates, add merely topical results, or merge facts and provenance from different candidate identities. Preserve exact structured-output requirements: if the user asks for JSON, return valid JSON with no Markdown fence or surrounding commentary. A paper_title or other provenance field must name one actual source, never a synthetic list of supporting papers. For reports, cite direct source URLs next to supported claims. Never invent missing values or citations; make uncertainty explicit when the requested format permits it."""

    @action(reasoning_effort="high", tools=[])
    async def compose(
        self,
        question: str,
        plan: dict,
        evidence_ledger: list[dict],
        evaluation: dict,
    ):
        """Return only the final user-facing deliverable. Satisfy the output contract exactly and use only evidence present in the ledger."""

    @action(reasoning_effort="medium", tools=[])
    async def revise(self, draft_audit: dict):
        """Revise the draft already present in this persistent Writer Session. Apply every audit repair instruction, preserve approved content, remove unapproved or synthetic candidates, and return only the corrected final deliverable in the exact requested format."""

    @action(reasoning_effort="high", tools=[])
    async def revise_from_human(
        self,
        guidance: HumanMessage,
        draft_audit: dict,
    ):
        """Revise the immediately preceding draft using the human's direct guidance and the latest failed audit. Do not add unsupported evidence or change the requested output contract. Return only the complete corrected deliverable."""


def _clean_text(value, fallback=""):
    text = str(value or "").strip()
    return text or fallback


def _normalize_output_contract(raw_contract, question):
    contract = _clean_text(
        raw_contract,
        "Follow the output format requested in the question exactly.",
    )
    question_text = str(question or "").casefold()
    contract_text = contract.casefold()
    invented_formats = [
        value
        for value in ("json", "jsonl", "yaml", "csv", "xml")
        if value in contract_text and value not in question_text
    ]
    if invented_formats:
        return (
            "The original question did not request a machine-readable format. "
            "Return a clear reader-facing report in prose/Markdown; do not introduce "
            "JSON, JSONL, YAML, CSV, XML, or a synthetic field schema."
        )
    return contract


def _answer_mode(raw_mode, question):
    requested = _clean_text(raw_mode).casefold()
    allowed = {
        "exact_match",
        "qualifying_list",
        "option_survey",
        "explanatory_report",
    }
    question_text = str(question or "").casefold()
    survey_markers = (
        "strategies",
        "best practices",
        "approaches",
        "implementation options",
        "existing projects",
        "possible solutions",
        "方案",
        "策略",
        "最佳实践",
        "现有项目",
    )
    if any(marker in question_text for marker in survey_markers):
        return "option_survey"
    list_markers = (
        "list all",
        "all items",
        "all candidates",
        "every candidate",
        "which ones",
        "列出所有",
        "所有符合",
        "全部符合",
    )
    if any(marker in question_text for marker in list_markers):
        return "qualifying_list"
    exact_markers = (
        "find one",
        "identify one",
        "identify the",
        "name of",
        "what is the name",
        "who is",
        "who was",
        "exact answer",
        "same candidate",
        "all of the following",
        "同时满足",
        "同一个",
        "名字",
        "名称",
        "是谁",
        "找出这个",
    )
    if any(marker in question_text for marker in exact_markers):
        return "exact_match"
    if requested in allowed:
        return requested
    return "explanatory_report"


def _plan_contract_error(raw, question, minimum_route_count, maximum_route_count):
    if not isinstance(raw, dict):
        return "planner output must be a JSON object"
    required_text = ("deliverable", "output_contract", "candidate_key")
    missing_text = [field for field in required_text if not _clean_text(raw.get(field))]
    if missing_text:
        return "missing non-empty fields: " + ", ".join(missing_text)
    requested_mode = _clean_text(raw.get("answer_mode")).casefold()
    if requested_mode not in {
        "exact_match",
        "qualifying_list",
        "option_survey",
        "explanatory_report",
    }:
        return "answer_mode is missing or invalid"
    expected_mode = _answer_mode(requested_mode, question)
    if requested_mode != expected_mode:
        return (
            f"answer_mode {requested_mode!r} conflicts with the request; "
            f"expected {expected_mode!r}"
        )

    coverage = raw.get("coverage_items")
    if not isinstance(coverage, list) or not coverage:
        return "coverage_items must be a non-empty array"
    coverage_ids = set()
    for index, item in enumerate(coverage[:16]):
        if not isinstance(item, dict):
            return f"coverage_items[{index}] must be an object"
        coverage_id = _clean_text(item.get("id"))
        if not coverage_id or not _clean_text(item.get("requirement")) or not _clean_text(item.get("acceptance_test")):
            return f"coverage_items[{index}] is missing id, requirement, or acceptance_test"
        if coverage_id in coverage_ids:
            return f"duplicate coverage id: {coverage_id}"
        coverage_ids.add(coverage_id)

    routes = raw.get("routes")
    if not isinstance(routes, list) or not (
        minimum_route_count <= len(routes) <= maximum_route_count
    ):
        return (
            "routes must contain between "
            f"{minimum_route_count} and {maximum_route_count} entries"
        )
    route_strategies = set()
    for index, route in enumerate(routes):
        if not isinstance(route, dict):
            return f"routes[{index}] must be an object"
        name = _clean_text(route.get("name"))
        objective = _clean_text(route.get("objective"))
        if not name or not objective:
            return f"routes[{index}] is missing name or objective"
        strategy_key = (name.casefold(), objective.casefold())
        if strategy_key in route_strategies:
            return "routes must use genuinely distinct names and objectives"
        route_strategies.add(strategy_key)
        assigned = route.get("coverage_ids")
        if not isinstance(assigned, list) or not assigned:
            return f"routes[{index}].coverage_ids must be a non-empty array"
        unknown = [str(value) for value in assigned if str(value) not in coverage_ids]
        if unknown:
            return f"routes[{index}] references unknown coverage ids: " + ", ".join(unknown)

    if expected_mode in {"exact_match", "qualifying_list"}:
        joint = raw.get("joint_constraints")
        if not isinstance(joint, list) or not any(_clean_text(value) for value in joint):
            return f"{expected_mode} requires explicit joint_constraints"
    if not isinstance(raw.get("verification_rules"), list):
        return "verification_rules must be an array"
    return ""


async def _plan_with_contract(
    planner,
    question,
    minimum_route_count,
    maximum_route_count,
    extra_requirements,
    prior_context_brief,
):
    recovery_instruction = ""
    last_error = ""
    for _attempt in range(2):
        raw = await planner.plan(
            question,
            minimum_route_count,
            maximum_route_count,
            extra_requirements,
            prior_context_brief,
            recovery_instruction,
        )
        last_error = _plan_contract_error(
            raw,
            question,
            minimum_route_count,
            maximum_route_count,
        )
        if not last_error:
            return _normalize_plan(
                raw,
                question,
                minimum_route_count,
                maximum_route_count,
                extra_requirements,
            )
        recovery_instruction = (
            "The previous response was not a valid research plan ("
            + last_error
            + "). Do not answer or research the user's question. Return the complete "
            "question-specific Planner JSON contract with every required field."
        )
    raise ValueError("planner failed semantic contract after retry: " + last_error)


def _normalize_plan(raw, question, minimum_route_count, maximum_route_count, extra_requirements):
    answer_mode = _answer_mode(raw.get("answer_mode"), question)
    coverage = []
    seen_ids = set()
    for index, item in enumerate(raw.get("coverage_items") or []):
        if not isinstance(item, dict):
            continue
        coverage_id = _clean_text(item.get("id"), f"coverage-{index + 1}")
        if coverage_id in seen_ids:
            coverage_id = f"{coverage_id}-{index + 1}"
        seen_ids.add(coverage_id)
        coverage.append(
            {
                "id": coverage_id,
                "requirement": _clean_text(item.get("requirement"), "Resolve this part of the request."),
                "acceptance_test": _clean_text(item.get("acceptance_test"), "Backed by adequate evidence."),
            }
        )
        if len(coverage) == 16:
            break
    if not coverage:
        coverage = [dict(item) for item in DEFAULT_COVERAGE]

    routes = []
    for index, item in enumerate(raw.get("routes") or []):
        if not isinstance(item, dict):
            continue
        coverage_ids = [
            str(value)
            for value in item.get("coverage_ids") or []
            if str(value) in seen_ids or any(str(value) == row["id"] for row in coverage)
        ]
        routes.append(
            {
                "name": _clean_text(item.get("name"), f"Route {index + 1}"),
                "objective": _clean_text(
                    item.get("objective"),
                    "Gather authoritative evidence for the assigned coverage items.",
                ),
                "coverage_ids": coverage_ids or [row["id"] for row in coverage],
            }
        )
        if len(routes) == maximum_route_count:
            break
    fallbacks = [
        ("Primary evidence", "Find the key entities, facts, exact values, and first-party or primary sources."),
        ("Completeness challenge", "Independently search for omitted qualifying items, counterexamples, and edge cases."),
        ("Cross-check", "Verify dates, numbers, names, output fields, contradictions, and source quality."),
        ("Alternative path", "Use different query formulations and source families to close remaining gaps."),
    ]
    while len(routes) < minimum_route_count:
        name, objective = fallbacks[len(routes)]
        routes.append(
            {
                "name": name,
                "objective": objective,
                "coverage_ids": [row["id"] for row in coverage],
            }
        )

    rules = [
        _clean_text(value)
        for value in raw.get("verification_rules") or []
        if _clean_text(value)
    ]
    rules.extend(extra_requirements)
    joint_constraints = []
    if answer_mode in {"exact_match", "qualifying_list"}:
        joint_constraints = [
            _clean_text(value)
            for value in raw.get("joint_constraints") or []
            if _clean_text(value)
        ]
    if answer_mode == "option_survey":
        joint_constraints.append(
            "Coverage requirements apply to the final portfolio of options, not to every option individually. Include evidence-backed strategies or tools that solve distinct subproblems, and label each option's role, prerequisites, limitations, and uncovered requirements without attributing another option's capabilities to it."
        )
    joint_constraints.extend(
        [
            "Requirements joined by words such as all, every, same, each, simultaneously, or together must hold for one stable candidate identity; never satisfy the conjunction by merging different candidates.",
            "Output provenance fields must identify real individual sources from the evidence ledger, not a synthetic title or collection assembled during writing.",
            "For list answers, unresolved or merely topical candidates stay out of the final list unless the question explicitly requests partial matches.",
        ]
    )
    return {
        "answer_mode": answer_mode,
        "deliverable": _clean_text(raw.get("deliverable"), "A complete answer to the user's request."),
        "output_contract": _normalize_output_contract(
            raw.get("output_contract"), question
        ),
        "candidate_key": _clean_text(
            raw.get("candidate_key"),
            "The stable identity of one output candidate requested by the user.",
        ),
        "coverage_items": coverage,
        "joint_constraints": joint_constraints,
        "routes": routes,
        "verification_rules": rules,
    }


def _follow_up_assignments(evaluation, route_count, limit):
    grouped = {}
    for item in evaluation.get("follow_ups") or []:
        if not isinstance(item, dict):
            continue
        try:
            route_index = int(item.get("route_index", 0))
        except (TypeError, ValueError):
            route_index = 0
        route_index = max(0, min(route_index, route_count - 1))
        objective = _clean_text(item.get("objective"))
        if not objective:
            continue
        current = grouped.setdefault(
            route_index,
            {"route_index": route_index, "objectives": [], "coverage_ids": []},
        )
        current["objectives"].append(objective)
        current["coverage_ids"].extend(
            str(value) for value in item.get("coverage_ids") or []
        )
        if len(grouped) == limit:
            break
    return list(grouped.values())


def _coverage_subset(coverage, coverage_ids):
    wanted = {str(value) for value in coverage_ids or []}
    selected = [item for item in coverage if str(item.get("id")) in wanted]
    return selected or list(coverage)


def _packet_contract_error(packet, coverage_contract):
    findings = packet.get("findings")
    if not isinstance(findings, list):
        return "findings must be an array"
    if not findings:
        return ""
    allowed = {str(item.get("id")) for item in coverage_contract}
    observed = set()
    for finding in findings:
        if not isinstance(finding, dict):
            continue
        observed.update(str(value) for value in finding.get("coverage_ids") or [])
    if not observed:
        return "non-empty findings did not identify any coverage_ids"
    if allowed.isdisjoint(observed):
        return (
            "all finding coverage_ids were unrelated to this route; expected one of: "
            + ", ".join(sorted(allowed))
        )
    return ""


def _normalize_draft_audit(raw):
    audit = dict(raw) if isinstance(raw, dict) else {}
    issue_fields = (
        "format_errors",
        "unsupported_outputs",
        "omitted_approved_candidates",
        "precision_errors",
    )
    for field in (*issue_fields, "repair_instructions"):
        value = audit.get(field)
        if isinstance(value, list):
            continue
        audit[field] = [] if value in (None, "") else [value]

    model_pass = audit.get("pass") is True
    has_issues = any(audit[field] for field in issue_fields)
    audit["model_pass"] = model_pass
    audit["pass"] = model_pass and not has_issues
    audit["revision_required"] = not audit["pass"]
    if model_pass and has_issues:
        audit["consistency_errors"] = [
            "The evaluator marked the draft as passing while reporting concrete errors. "
            "The runtime deterministically requires revision."
        ]
    else:
        audit["consistency_errors"] = []
    if has_issues and not audit["repair_instructions"]:
        audit["repair_instructions"] = [
            "Repair every reported draft-audit error without adding unsupported content."
        ]
    return audit


def _normalize_evaluation(raw, plan):
    evaluation = dict(raw) if isinstance(raw, dict) else {}
    for field in (
        "coverage",
        "candidate_decisions",
        "approved_candidates",
        "contradictions",
        "follow_ups",
    ):
        value = evaluation.get(field)
        if not isinstance(value, list):
            evaluation[field] = [] if value in (None, "") else [value]

    model_pass = evaluation.get("pass") is True
    consistency_errors = []
    if model_pass and evaluation["follow_ups"]:
        consistency_errors.append(
            "The evaluator marked the evidence as passing while requesting follow-up research."
        )
    if model_pass and evaluation.get("needs_human") is True:
        consistency_errors.append(
            "The evaluator marked the evidence as passing while requesting human review."
        )

    required_ids = {
        str(item.get("id"))
        for item in plan.get("coverage_items") or []
        if isinstance(item, dict) and item.get("id") is not None
    }
    covered_ids = {
        str(item.get("coverage_id"))
        for item in evaluation["coverage"]
        if isinstance(item, dict)
        and str(item.get("status", "")).casefold()
        in {"covered", "complete", "completed", "passed", "satisfied"}
    }
    missing_coverage = sorted(required_ids - covered_ids)
    if model_pass and missing_coverage:
        consistency_errors.append(
            "The evaluator pass omitted covered evidence for: "
            + ", ".join(missing_coverage)
        )

    if (
        model_pass
        and plan.get("answer_mode") in {"exact_match", "qualifying_list"}
        and not evaluation["approved_candidates"]
    ):
        consistency_errors.append(
            "An exact-match or qualifying-list pass requires at least one approved candidate."
        )

    evaluation["model_pass"] = model_pass
    evaluation["consistency_errors"] = consistency_errors
    evaluation["pass"] = model_pass and not consistency_errors
    if consistency_errors and not evaluation["follow_ups"]:
        evaluation["needs_human"] = True
    return evaluation


def _audit_policy(raw):
    value = _clean_text(raw, "deliver_with_warning").casefold()
    allowed = {"deliver_with_warning", "wait_for_human", "fail_run"}
    return value if value in allowed else "deliver_with_warning"


def _completion_reasons(evaluation, draft_audit):
    reasons = []
    if evaluation.get("pass") is not True:
        reasons.append("evidence_evaluation_incomplete")
    if evaluation.get("needs_human") is True:
        reasons.append("evaluator_requested_human")
    if draft_audit.get("pass") is not True:
        reasons.append("draft_audit_failed")
    return reasons


def _audit_summary(audit):
    details = []
    for field in (
        "format_errors",
        "unsupported_outputs",
        "omitted_approved_candidates",
        "precision_errors",
        "repair_instructions",
    ):
        values = audit.get(field) or []
        if values:
            details.append(field + ": " + "; ".join(str(value) for value in values))
    return "\n".join(details) or "The evaluator did not provide detailed errors."


async def _research_with_contract(
    researcher,
    question,
    objective,
    full_coverage_contract,
    assigned_coverage_ids,
    joint_constraints,
    verification_rules,
    prior_context_brief,
    phase,
):
    current_objective = objective
    last_error = ""
    for attempt in range(2):
        packet = await researcher.research(
            question,
            current_objective,
            full_coverage_contract,
            assigned_coverage_ids,
            joint_constraints,
            verification_rules,
            prior_context_brief,
            phase,
        )
        last_error = _packet_contract_error(packet, full_coverage_contract)
        if not last_error:
            return packet
        current_objective = (
            objective
            + "\n\nRecovery instruction: the previous response was off-topic or violated "
            + "the frozen coverage contract ("
            + last_error
            + "). Discard unrelated content, research the original objective with tools, "
            + "and use only the supplied coverage IDs."
        )
    raise ValueError("research packet failed semantic contract after retry: " + last_error)


@workflow(
    slug="evidence-loop",
    name="Evidence loop",
    description="Research a question through independent evidence routes, evaluate coverage, and follow up where needed.",
    params_schema={
        "type": "object",
        "properties": {
            "route_count": {"type": "integer", "minimum": 2, "maximum": 4},
            "minimum_route_count": {"type": "integer", "minimum": 2, "maximum": 4},
            "extra_requirements": {"type": "array", "items": {"type": "string"}},
            "max_rounds": {"type": "integer", "minimum": 1, "maximum": 4},
            "max_followups_per_round": {"type": "integer", "minimum": 1, "maximum": 4},
            "audit_policy": {
                "type": "string",
                "enum": ["deliver_with_warning", "wait_for_human", "fail_run"],
            },
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
    output_schema={
        "type": "object",
        "properties": {
            "report": {"type": "string"},
            "plan": {"type": "object"},
            "evaluation": {"type": "object"},
            "draft_audit": {"type": "object"},
            "rounds": {"type": "integer"},
            "evidence_ledger": {"type": "array"},
            "route_sessions_reused": {"type": "boolean"},
            "completion": {"type": "object"},
        },
        "required": [
            "report",
            "plan",
            "evaluation",
            "draft_audit",
            "rounds",
            "evidence_ledger",
            "route_sessions_reused",
            "completion",
        ],
    },
)
async def main(ctx):
    route_count = max(2, min(int(ctx.params.get("route_count", 2)), 4))
    minimum_route_count = max(
        2,
        min(int(ctx.params.get("minimum_route_count", 2)), route_count),
    )
    max_rounds = max(1, min(int(ctx.params.get("max_rounds", 2)), 4))
    max_followups = max(
        1,
        min(int(ctx.params.get("max_followups_per_round", 2)), 4),
    )
    extra_requirements = [
        _clean_text(value)
        for value in ctx.params.get("extra_requirements") or []
        if _clean_text(value)
    ]
    audit_policy = _audit_policy(ctx.params.get("audit_policy"))
    planner_model = str(ctx.params.get("planner_model") or "")
    research_model = str(ctx.params.get("research_model") or "")
    evaluator_model = str(ctx.params.get("evaluator_model") or "")
    writer_model = str(ctx.params.get("writer_model") or "")

    planner = Planner(name="Planner", model=planner_model)
    context_analyst = ContextAnalyst(name="Prior context", model=planner_model)
    evaluator = Evaluator(name="Evaluator", model=evaluator_model)
    writer = Writer(name="Writer", model=writer_model)

    prior_context_brief = ""
    if ctx.context:
        prior_context_brief = await context_analyst.distill(ctx.request, ctx.context)

    plan = await _plan_with_contract(
        planner,
        ctx.request,
        minimum_route_count,
        route_count,
        extra_requirements,
        prior_context_brief,
    )
    routes = plan["routes"]
    researchers = [
        Researcher(
            name=route["name"],
            role=route["objective"],
            model=research_model,
        )
        for route in routes
    ]
    team = Team("Evidence routes", *researchers)
    await team.activate()
    for researcher in researchers:
        await relate(
            researcher,
            evaluator,
            kind="reviewed_by",
            instructions="The evaluator checks the frozen coverage contract, exact output fields, source quality, completeness, and contradictions.",
        )
    await relate(
        planner,
        evaluator,
        kind="defines_contract_for",
        instructions="The evaluator must not silently change the frozen coverage contract.",
    )
    await relate(
        evaluator,
        writer,
        kind="briefs",
        instructions="Pass the coverage judgment and unresolved caveats without rewriting the evidence.",
    )

    ledger = []
    async with scope("Initial evidence routes", plan["deliverable"]):
        initial = await together(
            *(
                _research_with_contract(
                    researcher,
                    ctx.request,
                    route["objective"],
                    plan["coverage_items"],
                    route["coverage_ids"],
                    plan["joint_constraints"],
                    plan["verification_rules"],
                    prior_context_brief,
                    "initial",
                )
                for researcher, route in zip(researchers, routes)
            )
        )
        ledger.extend(initial)

    round_number = 1
    evaluation = _normalize_evaluation(
        await evaluator.assess(ctx.request, plan, ledger, round_number),
        plan,
    )
    reused_sessions = False

    while evaluation.get("pass") is not True and round_number < max_rounds:
        assignments = _follow_up_assignments(
            evaluation,
            len(researchers),
            max_followups,
        )
        if not assignments:
            break
        round_number += 1
        reused_sessions = True
        async with scope(
            f"Follow-up round {round_number}",
            _clean_text(evaluation.get("rationale"), "Close remaining evidence gaps."),
        ):
            follow_up_packets = await together(
                *(
                    _research_with_contract(
                        researchers[item["route_index"]],
                        ctx.request,
                        "\n".join(item["objectives"]),
                        plan["coverage_items"],
                        item["coverage_ids"],
                        plan["joint_constraints"],
                        [],
                        prior_context_brief,
                        "evaluator_follow_up",
                    )
                    for item in assignments
                )
            )
            ledger.extend(follow_up_packets)
        evaluation = _normalize_evaluation(
            await evaluator.assess(ctx.request, plan, ledger, round_number),
            plan,
        )

    report = await writer.compose(ctx.request, plan, ledger, evaluation)
    initial_draft_audit = _normalize_draft_audit(
        await evaluator.audit_draft(
            ctx.request,
            plan,
            evaluation,
            report,
        )
    )
    audit_history = [initial_draft_audit]
    if initial_draft_audit["revision_required"]:
        report = await writer.revise(initial_draft_audit)
        audit_history.append(
            _normalize_draft_audit(
                await evaluator.audit_draft(
                    ctx.request,
                    plan,
                    evaluation,
                    report,
                )
            )
        )

    human_revision_count = 0
    human_decision = None
    reasons = _completion_reasons(evaluation, audit_history[-1])
    if reasons and audit_policy == "fail_run":
        raise ValueError(
            "evidence-loop completion policy rejected the result: "
            + ", ".join(reasons)
        )

    while reasons and audit_policy == "wait_for_human":
        answer = await ask_human(
            "The research Workflow reached its automated quality gate with unresolved "
            "issues: "
            + ", ".join(reasons)
            + "\n\nLatest draft audit:\n"
            + _audit_summary(audit_history[-1])
            + "\n\nReply `/deliver` to accept the current report, `/fail` to fail the "
            "Workflow, or type revision guidance for the Writer. Revision guidance becomes "
            "a verified user Turn in the persistent Writer Session.",
            agent=writer,
        )
        decision = str(answer).strip().casefold()
        if decision == "/deliver":
            human_decision = "deliver"
            break
        if decision == "/fail":
            raise ValueError("human rejected the evidence-loop result at its quality gate")
        report = await writer.revise_from_human(answer, audit_history[-1])
        human_revision_count += 1
        audit_history.append(
            await evaluator.audit_draft(
                ctx.request,
                plan,
                evaluation,
                report,
            )
        )
        audit_history[-1] = _normalize_draft_audit(audit_history[-1])
        reasons = _completion_reasons(evaluation, audit_history[-1])

    draft_audit = {
        "pass": audit_history[-1]["pass"],
        "revision_performed": len(audit_history) > 1,
        "human_revision_count": human_revision_count,
        "initial": audit_history[0],
        "final": audit_history[-1],
        "attempts": audit_history,
    }
    final_reasons = _completion_reasons(evaluation, audit_history[-1])
    if not final_reasons:
        completion_status = "passed"
    elif human_decision == "deliver":
        completion_status = "human_accepted"
    else:
        completion_status = "warning"
    completion = {
        "status": completion_status,
        "policy": audit_policy,
        "quality_gate_pass": not final_reasons,
        "reasons": final_reasons,
        "human_decision": human_decision,
        "human_revision_count": human_revision_count,
    }
    return {
        "report": report,
        "plan": plan,
        "evaluation": evaluation,
        "draft_audit": draft_audit,
        "rounds": round_number,
        "evidence_ledger": ledger,
        "route_sessions_reused": reused_sessions,
        "completion": completion,
    }
