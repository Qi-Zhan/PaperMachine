from papermachine import Agent, action, workflow


class Grader(Agent):
    access = "model_only"
    role = "independent exact-answer grader"
    system_prompt = """Act only as a blinded answer-equivalence judge. Compare the final answer extractable from the submitted response with the supplied reference answer. Do not browse, solve the question again, repair the response, or award credit for background reasoning when the final answer is absent or ambiguous. Minor formatting differences and a small numerical margin may be accepted; any meaningful inconsistency or non-equivalence is incorrect."""

    @action(max_steps=1, reasoning_effort="medium", max_output_tokens=16_384)
    async def grade(
        self,
        question: str,
        correct_answer: str,
        response: str,
    ) -> dict:
        """Return a JSON object with extracted_final_answer (string or null), reasoning (a concise comparison only against correct_answer), correct (boolean), and confidence (the 0-100 confidence explicitly stated in response, or 100 if absent). Set correct=false when no unambiguous final answer can be extracted."""


@workflow(
    slug="short-answer-grader",
    name="Short-answer grader",
    description="Blindly judge one final short answer against a supplied reference answer in a separate no-tool Session.",
    input_schema={
        "type": "object",
        "properties": {
            "question": {"type": "string"},
            "correct_answer": {"type": "string"},
            "response": {"type": "string"},
            "grader_model": {"type": "string"},
        },
        "required": ["question", "correct_answer", "response"],
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
        "max_action_steps": 3,
        "max_total_tokens": 250000,
        "max_uncached_tokens": 100000,
        "max_hosted_search_calls": 0,
        "max_wall_time_seconds": 1800,
    },
)
async def main(ctx):
    grader = Grader(
        name="Independent short-answer grader",
        model=str(ctx.input.get("grader_model") or ""),
    )
    grading = await grader.grade(
        str(ctx.input["question"]),
        str(ctx.input["correct_answer"]),
        str(ctx.input["response"]),
    )
    return {"grading": grading}
