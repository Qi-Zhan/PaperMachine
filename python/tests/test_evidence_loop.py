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
