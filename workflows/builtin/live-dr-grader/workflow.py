from papermachine import Agent, action, workflow


class LiveDRJudge(Agent):
    access = "model_only"
    role = "blind LiveDRBench claim-equivalence judge"
    system_prompt = """Judge only equivalence between the supplied encrypted-benchmark reference values and the submitted prediction. Do not research, repair, or reward the prediction using outside knowledge.

This rubric is adapted from the upstream Microsoft LiveDRBench evaluators:
- Strings are equivalent when they preserve the same meaning, including common acronyms, shortened names, partial titles, harmless casing/spacing differences, or one being a semantic subset of the other. For people, allow missing middle names and switched first/last order.
- Numeric values are equivalent within one percent unless eval_info supplies a field-specific tolerance.
- URLs must identify the same URL; a shortened path of that same URL is acceptable.
- For dictionary fields, score 3 when nearly all important meaning is preserved, 2 when the main idea is correct with a minor omission or inaccuracy, 1 for weak overlap missing most essential meaning, and 0 for incorrect or unrelated content.
- For list-of-dictionary tasks, match a prediction to a reference using only eval_info.primary_keys, semantic equivalence, and eval_info.note or eval_info.evaluation_note. Do not reuse a reference row for two predictions.
- For SciFacts tasks, each reference field is a list of acceptable equivalent answers. Mark every reference row semantically matched by each predicted paper_title or material.
- For entities and prior-art titles, make one-to-one semantic matches and do not reuse a reference value.

Return indices into the supplied arrays, never copy reference answers into explanatory prose. Be conservative but do not reject an acronym or canonical short name merely because the reference uses a full title."""

    @action(
        reasoning_effort="high",
    )
    async def judge(
        self,
        category: str,
        ground_truth: object,
        prediction: object,
        eval_info: dict,
    ) -> dict:
        """Return {"evaluations": [...], "notes": [...]}.

For entities and prior-art, emit one evaluation per predicted item:
{"prediction_index": int, "ground_truth_index": int|null}.

For novel-datasets and flights, emit one evaluation per predicted dictionary:
{"prediction_index": int, "ground_truth_index": int|null, "field_scores": {<reference field>: 0|1|2|3}}.
For a single reference dictionary, ground_truth_index is always 0. Score every reference field, using 0 when the prediction omits it.

For scifacts-geo and scifacts-materials, emit one evaluation per predicted dictionary:
{"prediction_index": int, "equivalent_ground_truth_indices": {"paper_title": [int, ...], "material": [int, ...]}}.
Omit material only for scifacts-geo. Each index means the predicted scalar is semantically equivalent to one of that reference row's accepted variants."""


def _normalized_key(value):
    return "".join(character for character in str(value).casefold() if character.isalnum())


def _field(mapping, key, default=None):
    if not isinstance(mapping, dict):
        return default
    if key in mapping:
        return mapping[key]
    wanted = _normalized_key(key)
    for candidate, value in mapping.items():
        if _normalized_key(candidate) == wanted:
            return value
    return default


def _keys(mapping, ignored):
    ignored = {_normalized_key(key) for key in ignored}
    if not isinstance(mapping, dict):
        return []
    return [key for key in mapping if _normalized_key(key) not in ignored]


def _score(value):
    if isinstance(value, bool):
        return 3 if value else 0
    try:
        return max(0, min(int(value), 3))
    except (TypeError, ValueError):
        return 0


def _metrics(matched, predicted, expected):
    precision = matched / predicted if predicted else 0.0
    recall = matched / expected if expected else 0.0
    f1 = 2 * precision * recall / (precision + recall) if precision + recall else 0.0
    return {
        "precision": precision,
        "recall": recall,
        "f1": f1,
        "matched_claims": matched,
        "predicted_claims": predicted,
        "expected_claims": expected,
    }


def _evaluation_map(judgment):
    result = {}
    for item in judgment.get("evaluations") or []:
        if not isinstance(item, dict):
            continue
        try:
            index = int(item.get("prediction_index"))
        except (TypeError, ValueError):
            continue
        result[index] = item
    return result


def _grade_names(ground_truth, prediction, evaluations, prior_art=False):
    if prior_art:
        expected = [item.get("title") for item in ground_truth if isinstance(item, dict)]
        predicted = [item.get("title") for item in prediction if isinstance(item, dict)]
    else:
        expected = list(ground_truth)
        predicted = list(prediction)
    used = set()
    matched = 0
    for index in range(len(predicted)):
        item = evaluations.get(index) or {}
        try:
            target = int(item.get("ground_truth_index"))
        except (TypeError, ValueError):
            continue
        if 0 <= target < len(expected) and target not in used:
            used.add(target)
            matched += 1
    return _metrics(matched, len(predicted), len(expected))


def _grade_dict(reference, prediction, evaluations, eval_info):
    ignored = eval_info.get("ignore_keys") or []
    reference_keys = _keys(reference, ignored)
    predicted_keys = _keys(prediction, ignored)
    item = evaluations.get(0) or {}
    field_scores = item.get("field_scores") or {}
    main_claims = eval_info.get("main_claims") or []
    identification_pass = all(_score(_field(field_scores, key, 0)) > 1 for key in main_claims)
    matched = 0
    if identification_pass:
        matched = sum(_score(_field(field_scores, key, 0)) > 1 for key in reference_keys)
    return _metrics(matched, len(predicted_keys), len(reference_keys))


def _grade_list_dicts(reference, prediction, evaluations, eval_info):
    ignored = eval_info.get("ignore_keys") or []
    main_claims = eval_info.get("main_claims") or []
    expected_claims = sum(len(_keys(item, ignored)) for item in reference if isinstance(item, dict))
    predicted_claims = sum(len(_keys(item, ignored)) for item in prediction if isinstance(item, dict))
    used = set()
    matched = 0
    for index, predicted_item in enumerate(prediction):
        if not isinstance(predicted_item, dict):
            continue
        item = evaluations.get(index) or {}
        try:
            target_index = int(item.get("ground_truth_index"))
        except (TypeError, ValueError):
            continue
        if not 0 <= target_index < len(reference) or target_index in used:
            continue
        target = reference[target_index]
        if not isinstance(target, dict):
            continue
        used.add(target_index)
        field_scores = item.get("field_scores") or {}
        if any(_score(_field(field_scores, key, 0)) <= 1 for key in main_claims):
            continue
        matched += sum(
            _score(_field(field_scores, key, 0)) > 1 for key in _keys(target, ignored)
        )
    return _metrics(matched, predicted_claims, expected_claims)


def _grade_scifact_key(reference, prediction, evaluations, key):
    predicted_indices = [
        index
        for index, item in enumerate(prediction)
        if isinstance(item, dict) and key in item
    ]
    matched_predictions = 0
    covered = set()
    for index in predicted_indices:
        item = evaluations.get(index) or {}
        mapping = item.get("equivalent_ground_truth_indices") or {}
        values = _field(mapping, key, [])
        valid = set()
        for value in values if isinstance(values, list) else []:
            try:
                target = int(value)
            except (TypeError, ValueError):
                continue
            if 0 <= target < len(reference):
                valid.add(target)
        if valid:
            matched_predictions += 1
            covered.update(valid)
    precision = matched_predictions / len(predicted_indices) if predicted_indices else 0.0
    recall = len(covered) / len(reference) if reference else 0.0
    f1 = 2 * precision * recall / (precision + recall) if precision + recall else 0.0
    return {
        "precision": precision,
        "recall": recall,
        "f1": f1,
        "matched_claims": min(matched_predictions, len(covered)),
        "predicted_claims": len(predicted_indices),
        "expected_claims": len(reference),
    }


def _grade(category, ground_truth, prediction, eval_info, judgment):
    evaluations = _evaluation_map(judgment)
    if category == "entities":
        return _grade_names(ground_truth, prediction, evaluations)
    if category == "prior-art":
        return _grade_names(ground_truth, prediction, evaluations, prior_art=True)
    if category == "scifacts-geo":
        return _grade_scifact_key(ground_truth, prediction, evaluations, "paper_title")
    if category == "scifacts-materials":
        paper = _grade_scifact_key(ground_truth, prediction, evaluations, "paper_title")
        material = _grade_scifact_key(ground_truth, prediction, evaluations, "material")
        return {
            "precision": paper["precision"] * material["precision"],
            "recall": paper["recall"] * material["recall"],
            "f1": paper["f1"] * material["f1"],
            "matched_claims": min(paper["matched_claims"], material["matched_claims"]),
            "predicted_claims": max(paper["predicted_claims"], material["predicted_claims"]),
            "expected_claims": len(ground_truth),
            "per_key": {"paper_title": paper, "material": material},
        }
    if isinstance(ground_truth, dict):
        return _grade_dict(ground_truth, prediction, evaluations, eval_info)
    return _grade_list_dicts(ground_truth, prediction, evaluations, eval_info)


@workflow(
    slug="live-dr-grader",
    name="LiveDRBench grader",
    description="Blindly apply the upstream LiveDRBench semantic claim-matching rubric, then compute precision, recall, and F1 deterministically.",
    params_schema={
        "type": "object",
        "properties": {
            "category": {"type": "string"},
            "ground_truth": {},
            "prediction": {},
            "eval_info": {"type": "object"},
            "grader_model": {
                "type": "string",
                "format": "model-profile",
                "title": "Grader model",
                "description": "Optional model profile for the Judge; empty inherits the Run model.",
            },
        },
        "required": ["category", "ground_truth", "prediction", "eval_info"],
        "additionalProperties": False,
    },
    output_schema={
        "type": "object",
        "properties": {"grading": {"type": "object"}},
        "required": ["grading"],
    },
)
async def main(ctx):
    category = str(ctx.params["category"])
    ground_truth = ctx.params["ground_truth"]
    prediction = ctx.params["prediction"]
    eval_info = dict(ctx.params.get("eval_info") or {})
    judge = LiveDRJudge(
        name="Blind claim judge",
        model=str(ctx.params.get("grader_model") or ""),
    )
    judgment = await judge.judge(category, ground_truth, prediction, eval_info)
    metrics = _grade(category, ground_truth, prediction, eval_info, judgment)
    return {
        "grading": {
            **metrics,
            "category": category,
            "judgment": judgment,
        }
    }
