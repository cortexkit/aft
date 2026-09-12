import json
import struct
import tempfile
import unittest
from pathlib import Path

from run_embed_body_cap_ab import (
    arm_definitions,
    most_moved,
    semantic_bin_metrics,
    validate_arm_contracts,
)


def packed_string(value: str) -> bytes:
    encoded = value.encode("utf-8")
    return struct.pack("<I", len(encoded)) + encoded


def semantic_entry(kind: int, embed_text: str, dimension: int = 2) -> bytes:
    return b"".join(
        [
            packed_string("src/main.rs"),
            packed_string("main"),
            packed_string("crate.main"),
            bytes([kind]),
            struct.pack("<II", 0, 2),
            b"\x01",
            packed_string("fn main() {}"),
            packed_string(embed_text),
            struct.pack("<ff", 0.0, 1.0),
        ]
    )


class EmbedBodyCapRunnerTests(unittest.TestCase):
    def test_arm_definitions_keep_control_exact_and_derive_remote_caps(self) -> None:
        control, medium, long = arm_definitions()
        self.assertEqual((control.max_input_tokens, control.body_lines, control.effective_body_chars, control.total_chars), (None, 15, 300, 1600))
        self.assertEqual((medium.max_input_tokens, medium.body_lines, medium.effective_body_chars, medium.total_chars), (531, None, 1001, 1858))
        self.assertEqual((long.max_input_tokens, long.body_lines, long.effective_body_chars, long.total_chars), (960, None, 2503, 3360))

    def test_semantic_bin_metrics_count_utf8_characters_and_rows(self) -> None:
        fingerprint = json.dumps({"embed_text_caps": {"body_chars": 1001}}).encode()
        file_table = packed_string("src/main.rs") + bytes(8 + 4 + 8 + 32)
        payload = b"".join(
            [
                b"\x07",
                struct.pack("<II", 2, 2),
                struct.pack("<I", len(fingerprint)),
                fingerprint,
                struct.pack("<I", 1),
                file_table,
                semantic_entry(9, "file:résumé"),
                semantic_entry(0, "body:λ"),
            ]
        )
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "semantic.bin"
            path.write_bytes(payload)
            metrics = semantic_bin_metrics(path)
        self.assertEqual(metrics["rows"], 2)
        self.assertEqual(metrics["file_summary_rows"], 1)
        self.assertEqual(metrics["symbol_rows"], 1)
        self.assertEqual(metrics["total_embed_chars"], len("file:résumé") + len("body:λ"))
        self.assertEqual(metrics["fingerprint"]["embed_text_caps"]["body_chars"], 1001)

    def test_most_moved_reports_rank_changes_in_both_directions(self) -> None:
        def arm(first_rank, second_rank, name="arm"):
            return {
                "arm": {"name": name},
                "families": {
                    "concept": {
                        "rows": [
                            {"query_key": "q1", "query": "one", "expected": "a", "top_k": 10, "rank": first_rank, "result_files": []},
                            {"query_key": "q2", "query": "two", "expected": "b", "top_k": 10, "rank": second_rank, "result_files": []},
                        ]
                    }
                }
            }

        moved = most_moved(arm(5, 1), arm(1, 4))
        self.assertEqual(moved["improved"][0]["query_key"], "q1")
        self.assertEqual(moved["improved"][0]["rank_delta"], 4)
        self.assertEqual(moved["worsened"][0]["query_key"], "q2")
        self.assertEqual(moved["worsened"][0]["rank_delta"], -3)
        validate_arm_contracts([arm(5, 1, "control"), arm(1, 4, "treatment")])
        changed_top_k = arm(1, 4, "bad")
        changed_top_k["families"]["concept"]["rows"][0]["top_k"] = 5
        with self.assertRaisesRegex(ValueError, "query or top-k contract"):
            validate_arm_contracts([arm(5, 1, "control"), changed_top_k])


if __name__ == "__main__":
    unittest.main()
