from papermachine import Agent, action, workflow


class Grader(Agent):
    access = "model_only"
    role = "independent post-write research report grader"
    system_prompt = """You are a strict, blinded evaluator. Grade only the supplied final report against every supplied criterion. You did not participate in the research and must not infer credit from hidden work. Do not browse, repair the report, or use a workflow's internal evaluator result. Unsupported, stale, contradictory, or uncited claims must not receive full credit. Apply the same 0-10 scale consistently across reports."""

    @action(max_steps=1, reasoning_effort="medium", max_output_tokens=32_768)
    async def grade(
        self,
        question: str,
        report: str,
        criteria: dict,
        required_counts: dict,
        language: str,
    ) -> dict:
        """Evaluate the final report point by point. Return one top-level array under each exact dimension key in required_counts, with exactly the stated number of items. Do not wrap the arrays in evaluations or dimensions. For every criterion in the supplied criteria object, return one item in the original order with criterion_index (zero-based integer), score (number from 0 through 10), and analysis (a concise evidence-based explanation). Use 0-2 for almost entirely missing, 2-4 for major deficiencies, 4-6 for basic/average fulfillment, 6-8 for good fulfillment with limitations, and 8-10 only for complete or outstanding fulfillment. Also return overall_assessment (string) and major_weaknesses (array of strings). Write analysis in the report's language. Return every criterion exactly once."""

    @action(max_steps=1, reasoning_effort="medium", max_output_tokens=32_768)
    async def repair_contract(
        self,
        invalid_grading: dict,
        validation_errors: list,
        criteria: dict,
        required_counts: dict,
        language: str,
    ) -> dict:
        """Repair the immediately preceding grading result without changing its substantive judgments. Return one top-level array under each exact dimension key in required_counts, with exactly the stated number of items and no evaluations wrapper. Every item must contain criterion_index, score, and a non-empty analysis. Restore missing judgments from the report and rubric already present in this Session when necessary. Also return overall_assessment and major_weaknesses. Return the complete grading object, not a patch."""


def _normalize_grading(raw, criteria):
    grading = dict(raw) if isinstance(raw, dict) else {}
    dimensions = grading.get("dimensions")
    if isinstance(dimensions, dict):
        for dimension in criteria:
            if dimension not in grading and dimension in dimensions:
                grading[dimension] = dimensions[dimension]

    evaluations = grading.get("evaluations")
    if isinstance(evaluations, list):
        grouped = {dimension: [] for dimension in criteria}
        for rating in evaluations:
            if not isinstance(rating, dict):
                continue
            dimension = rating.get("dimension")
            if dimension in grouped:
                grouped[dimension].append(rating)
        for dimension, ratings in grouped.items():
            if not isinstance(grading.get(dimension), list) and ratings:
                grading[dimension] = ratings

    for dimension, expected in criteria.items():
        ratings = grading.get(dimension)
        if not isinstance(ratings, list):
            continue
        normalized = []
        for position, rating in enumerate(ratings):
            if not isinstance(rating, dict):
                normalized.append(rating)
                continue
            item = dict(rating)
            if "criterion_index" not in item and len(ratings) == len(expected):
                item["criterion_index"] = position
            normalized.append(item)
        grading[dimension] = normalized

    weaknesses = grading.get("major_weaknesses")
    if isinstance(weaknesses, str):
        grading["major_weaknesses"] = [weaknesses]
    return grading


def _uses_alternate_shape(raw, criteria):
    if not isinstance(raw, dict):
        return False
    if isinstance(raw.get("evaluations"), list) or isinstance(raw.get("dimensions"), dict):
        return any(not isinstance(raw.get(dimension), list) for dimension in criteria)
    return False


def _grading_contract_errors(grading, criteria):
    if not isinstance(grading, dict):
        return ["grading must be an object"]

    errors = []
    for dimension, expected in criteria.items():
        ratings = grading.get(dimension)
        if not isinstance(ratings, list):
            errors.append(f"{dimension} must be an array")
            continue
        if len(ratings) != len(expected):
            errors.append(
                f"{dimension} must contain {len(expected)} ratings, got {len(ratings)}"
            )
            continue

        seen = set()
        for position, rating in enumerate(ratings):
            if not isinstance(rating, dict):
                errors.append(f"{dimension}[{position}] must be an object")
                continue
            index = rating.get("criterion_index")
            if isinstance(index, bool) or not isinstance(index, int):
                errors.append(f"{dimension}[{position}] has an invalid criterion_index")
            elif index < 0 or index >= len(expected) or index in seen:
                errors.append(f"{dimension}[{position}] has duplicate or out-of-range index {index}")
            else:
                seen.add(index)
            score = rating.get("score")
            if (
                isinstance(score, bool)
                or not isinstance(score, (int, float))
                or not 0 <= float(score) <= 10
            ):
                errors.append(f"{dimension}[{position}] has a score outside 0..10")
            if not isinstance(rating.get("analysis"), str) or not rating["analysis"].strip():
                errors.append(f"{dimension}[{position}] must include a non-empty analysis")
        if seen != set(range(len(expected))):
            errors.append(f"{dimension} criterion indices are incomplete")

    if not isinstance(grading.get("overall_assessment"), str) or not grading[
        "overall_assessment"
    ].strip():
        errors.append("overall_assessment must be a non-empty string")
    weaknesses = grading.get("major_weaknesses")
    if not isinstance(weaknesses, list) or not all(
        isinstance(item, str) and item.strip() for item in weaknesses
    ):
        errors.append("major_weaknesses must be an array of non-empty strings")
    return errors


@workflow(
    slug="report-grader",
    name="Report grader",
    description="Blindly grade one completed report against a full external rubric in a separate no-tool Session, with deterministic contract validation and bounded in-Session repair.",
    params_schema={
        "type": "object",
        "properties": {
            "question": {"type": "string"},
            "report": {"type": "string"},
            "criteria": {"type": "object"},
            "language": {"type": "string"},
            "grader_model": {
                "type": "string",
                "format": "model-profile",
                "title": "Grader model",
                "description": "Optional model profile for the Grader; empty inherits the Run model.",
            },
        },
        "required": ["question", "report", "criteria", "language"],
        "additionalProperties": False,
    },
    output_schema={
        "type": "object",
        "properties": {"grading": {"type": "object"}},
        "required": ["grading"],
    },
    budget={
        "max_agents": 1,
        "max_concurrent_actions": 1,
        "max_action_steps": 9,
        "max_total_tokens": 750000,
        "max_uncached_tokens": 250000,
        "max_hosted_search_calls": 0,
        "max_wall_time_seconds": 3600,
    },
)
async def main(ctx):
    criteria = dict(ctx.params["criteria"])
    required_counts = {
        dimension: len(items) if isinstance(items, list) else 0
        for dimension, items in criteria.items()
    }
    grader = Grader(
        name="Independent grader",
        model=str(ctx.params.get("grader_model") or ""),
    )
    grading = await grader.grade(
        str(ctx.params["question"]),
        str(ctx.params["report"]),
        criteria,
        required_counts,
        str(ctx.params["language"]),
    )
    alternate_shape_normalized = _uses_alternate_shape(grading, criteria)
    grading = _normalize_grading(grading, criteria)
    repair_attempts = 0
    errors = _grading_contract_errors(grading, criteria)
    while errors and repair_attempts < 2:
        repair_attempts += 1
        grading = await grader.repair_contract(
            grading,
            errors,
            criteria,
            required_counts,
            str(ctx.params["language"]),
        )
        alternate_shape_normalized = alternate_shape_normalized or _uses_alternate_shape(
            grading, criteria
        )
        grading = _normalize_grading(grading, criteria)
        errors = _grading_contract_errors(grading, criteria)
    if errors:
        raise ValueError("grader contract remained invalid: " + "; ".join(errors))
    return {
        "grading": grading,
        "contract": {
            "validated": True,
            "alternate_shape_normalized": alternate_shape_normalized,
            "semantic_repair_attempts": repair_attempts,
        },
    }
