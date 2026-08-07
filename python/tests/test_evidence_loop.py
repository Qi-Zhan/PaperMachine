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
packet_contract_error = WORKFLOW["_packet_contract_error"]
normalize_plan = WORKFLOW["_normalize_plan"]
normalize_output_contract = WORKFLOW["_normalize_output_contract"]
normalize_draft_audit = WORKFLOW["_normalize_draft_audit"]
normalize_evaluation = WORKFLOW["_normalize_evaluation"]
plan_contract_error = WORKFLOW["_plan_contract_error"]
audit_policy = WORKFLOW["_audit_policy"]
completion_reasons = WORKFLOW["_completion_reasons"]


class EvidencePacketContractTests(unittest.TestCase):
    def test_accepts_findings_assigned_to_this_route(self) -> None:
        error = packet_contract_error(
            {"findings": [{"coverage_ids": ["mechanism"]}]},
            [{"id": "mechanism"}, {"id": "limits"}],
        )
        self.assertEqual(error, "")

    def test_rejects_cross_request_completion(self) -> None:
        error = packet_contract_error(
            {"findings": [{"coverage_ids": ["drive-remove-user"]}]},
            [{"id": "a2a-architecture"}, {"id": "mcp-architecture"}],
        )
        self.assertIn("unrelated", error)
        self.assertIn("a2a-architecture", error)

    def test_allows_evidence_free_packet_to_express_unresolved_gap(self) -> None:
        error = packet_contract_error(
            {"findings": [], "gaps": ["Primary source unavailable"]},
            [{"id": "primary-source"}],
        )
        self.assertEqual(error, "")


class EvidencePlanContractTests(unittest.TestCase):
    def valid_exact_plan(self) -> dict:
        return {
            "answer_mode": "exact_match",
            "deliverable": "Identify the one matching person.",
            "output_contract": "Return the requested explanation and exact answer.",
            "candidate_key": "person name",
            "coverage_items": [
                {
                    "id": "identity",
                    "requirement": "One person satisfies every clue.",
                    "acceptance_test": "Every clue is verified for the same person.",
                }
            ],
            "joint_constraints": ["Every clue belongs to the same person."],
            "routes": [
                {
                    "name": "Exact phrase search",
                    "objective": "Search rare clue combinations across primary sources.",
                    "coverage_ids": ["identity"],
                },
                {
                    "name": "Independent identity challenge",
                    "objective": "Generate alternatives and try to falsify each complete match.",
                    "coverage_ids": ["identity"],
                },
            ],
            "verification_rules": ["Verify the identifying statement directly."],
        }

    def test_name_question_is_exact_match_even_if_planner_calls_it_report(self) -> None:
        plan = self.valid_exact_plan()
        plan["answer_mode"] = "explanatory_report"

        error = plan_contract_error(
            plan,
            "Can you tell me the name of the one person matching every clue?",
            2,
            2,
        )

        self.assertIn("expected 'exact_match'", error)

    def test_final_answer_shaped_planner_output_fails_closed(self) -> None:
        error = plan_contract_error(
            {
                "Explanation": "A guessed answer rather than a plan.",
                "Exact Answer": "Someone",
                "Confidence": "50%",
            },
            "What is the name of the matching person?",
            2,
            2,
        )

        self.assertIn("missing non-empty fields", error)

    def test_valid_question_specific_plan_passes_contract(self) -> None:
        self.assertEqual(
            plan_contract_error(
                self.valid_exact_plan(),
                "What is the name of the one person matching every clue?",
                2,
                2,
            ),
            "",
        )

    def test_normalization_preserves_joint_candidate_constraints(self) -> None:
        plan = normalize_plan(
            {
                "candidate_key": "material formula",
                "coverage_items": [
                    {
                        "id": "gap",
                        "requirement": "Match the band gap",
                        "acceptance_test": "The same material has the measured gap",
                    }
                ],
                "joint_constraints": [
                    "Every measured property must belong to the same material."
                ],
                "routes": [
                    {
                        "name": "Exact clues",
                        "objective": "Search the rare value combination",
                        "coverage_ids": ["gap"],
                    },
                    {
                        "name": "Primary sources",
                        "objective": "Verify candidate papers",
                        "coverage_ids": ["gap"],
                    },
                ],
            },
            "Find one material matching all constraints.",
            2,
            2,
            [],
        )

        self.assertEqual(plan["candidate_key"], "material formula")
        self.assertIn(
            "Every measured property must belong to the same material.",
            plan["joint_constraints"],
        )
        self.assertTrue(
            any("one stable candidate" in item for item in plan["joint_constraints"])
        )

    def test_planner_cannot_invent_json_output_contract(self) -> None:
        contract = normalize_output_contract(
            "Return a JSON object with plugins and synthesis fields.",
            "Compare Obsidian plugins and explain their strengths and weaknesses.",
        )

        self.assertIn("reader-facing report", contract)
        self.assertIn("did not request", contract)

    def test_explicit_json_request_is_preserved(self) -> None:
        contract = normalize_output_contract(
            "Return a JSON object with answer and sources fields.",
            "Research the question and return JSON with answer and sources.",
        )

        self.assertEqual(
            contract,
            "Return a JSON object with answer and sources fields.",
        )

    def test_option_survey_drops_all_or_nothing_candidate_constraints(self) -> None:
        plan = normalize_plan(
            {
                "answer_mode": "exact_match",
                "joint_constraints": [
                    "Every accepted project must satisfy all twelve coverage items."
                ],
                "coverage_items": [
                    {
                        "id": "scheduled",
                        "requirement": "Cover scheduled scaling.",
                        "acceptance_test": "A schedule is documented.",
                    },
                    {
                        "id": "predictive",
                        "requirement": "Cover predictive scaling.",
                        "acceptance_test": "A forecast is documented.",
                    },
                ],
                "routes": [],
            },
            "What implementation strategies, best practices, or existing projects address predictive or scheduled node autoscaling?",
            2,
            2,
            [],
        )

        self.assertEqual(plan["answer_mode"], "option_survey")
        self.assertFalse(
            any("all twelve" in item for item in plan["joint_constraints"])
        )
        self.assertTrue(
            any("final portfolio" in item for item in plan["joint_constraints"])
        )


class DraftAuditContractTests(unittest.TestCase):
    def test_evaluator_followups_override_an_inconsistent_pass(self) -> None:
        evaluation = normalize_evaluation(
            {
                "pass": True,
                "coverage": [
                    {"coverage_id": "identity", "status": "covered"}
                ],
                "approved_candidates": [{"candidate_id": "candidate-a"}],
                "follow_ups": [
                    {
                        "route_index": 0,
                        "objective": "Verify the identifying source directly.",
                        "coverage_ids": ["identity"],
                    }
                ],
            },
            {
                "answer_mode": "exact_match",
                "coverage_items": [{"id": "identity"}],
            },
        )

        self.assertTrue(evaluation["model_pass"])
        self.assertFalse(evaluation["pass"])
        self.assertEqual(len(evaluation["consistency_errors"]), 1)

    def test_completion_reasons_cover_evidence_human_and_draft_gates(self) -> None:
        reasons = completion_reasons(
            {"pass": False, "needs_human": True},
            {"pass": False},
        )

        self.assertEqual(
            reasons,
            [
                "evidence_evaluation_incomplete",
                "evaluator_requested_human",
                "draft_audit_failed",
            ],
        )

    def test_unknown_audit_policy_falls_back_to_warning(self) -> None:
        self.assertEqual(audit_policy("FAIL_RUN"), "fail_run")
        self.assertEqual(audit_policy("invented"), "deliver_with_warning")

    def test_clean_model_pass_remains_pass(self) -> None:
        audit = normalize_draft_audit(
            {
                "pass": True,
                "format_errors": [],
                "unsupported_outputs": [],
                "omitted_approved_candidates": [],
                "precision_errors": [],
                "repair_instructions": [],
            }
        )

        self.assertTrue(audit["pass"])
        self.assertFalse(audit["revision_required"])
        self.assertEqual(audit["consistency_errors"], [])

    def test_reported_error_overrides_inconsistent_model_pass(self) -> None:
        audit = normalize_draft_audit(
            {
                "pass": True,
                "precision_errors": [{"issue": "Wrong source attribution"}],
                "repair_instructions": ["Correct the attribution."],
            }
        )

        self.assertTrue(audit["model_pass"])
        self.assertFalse(audit["pass"])
        self.assertTrue(audit["revision_required"])
        self.assertEqual(len(audit["consistency_errors"]), 1)

    def test_missing_array_fields_are_normalized(self) -> None:
        audit = normalize_draft_audit(
            {"pass": False, "format_errors": "Return valid JSON."}
        )

        self.assertEqual(audit["format_errors"], ["Return valid JSON."])
        self.assertEqual(audit["unsupported_outputs"], [])
        self.assertTrue(audit["repair_instructions"])
        self.assertTrue(audit["revision_required"])

if __name__ == "__main__":
    unittest.main()
