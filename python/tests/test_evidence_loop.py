from __future__ import annotations

import runpy
import unittest
from pathlib import Path


WORKFLOW = runpy.run_path(
    Path(__file__).resolve().parents[2]
    / "workflows"
    / "builtin"
    / "evidence-loop"
    / "workflow.py"
)
create_plan = WORKFLOW["_create_plan"]
assessment_error = WORKFLOW["_assessment_error"]
follow_ups = WORKFLOW["_follow_ups"]


def valid_plan() -> dict:
    return {
        "deliverable": "Answer the complete request with cited evidence.",
        "acceptance_criteria": ["Every requested part is answered."],
        "routes": [
            {"name": "Primary sources", "objective": "Find direct evidence."},
            {"name": "Challenge", "objective": "Seek counterevidence and omissions."},
        ],
        "verification_notes": [],
    }


class PlannerStub:
    def __init__(self, responses: list[dict]):
        self.responses = iter(responses)
        self.feedback: list[str] = []

    async def plan(
        self,
        _question: str,
        _route_count: int,
        _extra_requirements: list[str],
        _prior_context_brief: str,
        feedback: str,
    ) -> dict:
        self.feedback.append(feedback)
        return next(self.responses)


class EvidenceLoopStructureTests(unittest.IsolatedAsyncioTestCase):
    async def test_planner_gets_one_self_correction_turn_without_fallback_routes(self) -> None:
        planner = PlannerStub([{"deliverable": "Incomplete", "routes": []}, valid_plan()])

        plan = await create_plan(planner, "Question", 2, [], "")

        self.assertEqual(plan, valid_plan())
        self.assertEqual(planner.feedback[0], "")
        self.assertIn("exactly 2", planner.feedback[1])

    def test_assessment_requires_consistent_completion_and_real_route_indices(self) -> None:
        self.assertEqual(
            assessment_error(
                {
                    "complete": False,
                    "follow_ups": [{"route_index": 1, "objective": "Verify the date."}],
                },
                2,
            ),
            "",
        )
        self.assertIn(
            "complete assessment",
            assessment_error(
                {
                    "complete": True,
                    "follow_ups": [{"route_index": 0, "objective": "Search again."}],
                },
                2,
            ),
        )
        self.assertIn(
            "outside the route list",
            assessment_error(
                {
                    "complete": False,
                    "follow_ups": [{"route_index": 3, "objective": "Search again."}],
                },
                2,
            ),
        )

    def test_follow_up_limit_preserves_evaluator_order(self) -> None:
        requested = [
            {"route_index": 1, "objective": "First"},
            {"route_index": 0, "objective": "Second"},
        ]

        self.assertEqual(follow_ups({"follow_ups": requested}, 1), requested[:1])


if __name__ == "__main__":
    unittest.main()
