import struct
import tempfile
import unittest
from pathlib import Path
from unittest.mock import MagicMock, patch

from run_query_instruction_ab import (
    CODE_SEARCH_TASK,
    MODEL_CARD_TASK,
    aggregate_dense,
    arm_definitions,
    content_tokens,
    query_text,
    resolve_no_vocabulary_band,
)


class QueryInstructionAbRunnerTests(unittest.TestCase):
    def test_three_arms_pin_off_model_card_and_code_search_text(self) -> None:
        off, model_card, code_search = arm_definitions()
        self.assertEqual([off.name, model_card.name, code_search.name], ["off", "model-card", "code-search"])
        self.assertEqual(query_text(off, "needle"), "needle")
        self.assertEqual(query_text(model_card, "needle"), f"Instruct: {MODEL_CARD_TASK}\nQuery: needle")
        self.assertEqual(query_text(code_search, "needle"), f"Instruct: {CODE_SEARCH_TASK}\nQuery: needle")

    def test_content_tokens_match_exact_lane_stopword_and_identifier_rules(self) -> None:
        self.assertEqual(content_tokens("Where does Foo.bar handle the _id?"), {"foo.bar", "handle", "_id"})

    def test_no_vocabulary_aggregate_reports_share_relevance_and_latency(self) -> None:
        aggregate = aggregate_dense([
            {
                "dense_top_10_rows": 10,
                "no_vocabulary_rows": 4,
                "relevant_no_vocabulary_rows": 1,
                "query_embed_latency_ms": 10.0,
            },
            {
                "dense_top_10_rows": 10,
                "no_vocabulary_rows": 6,
                "relevant_no_vocabulary_rows": 2,
                "query_embed_latency_ms": 20.0,
            },
        ])
        self.assertEqual(aggregate["share_of_dense_top_10"], 0.5)
        self.assertEqual(aggregate["relevance_rate_inside_band"], 0.3)
        self.assertEqual(aggregate["query_embed_latency_ms_p50"], 10.0)
        self.assertEqual(aggregate["query_embed_latency_ms_p95"], 20.0)

    def test_semantic_versions_and_v8_skipped_file_trailer(self) -> None:
        path = b"src/refused.rs"
        field = struct.pack("<I", len(path)) + path
        # No vectors are needed to test format parsing; keep this test stdlib-only.
        numpy = MagicMock()
        numpy.isfinite.return_value.all.return_value = True
        with tempfile.TemporaryDirectory() as directory, patch.dict("sys.modules", {"numpy": numpy}):
            index = Path(directory) / "semantic.bin"
            for version, refused in [(7, False), (8, False), (8, True)]:
                with self.subTest(version=version, refused=refused):
                    data = struct.pack("<BIIII", version, 1, 0, 0, 1) + field + bytes(8 + 4 + 8 + 32)
                    if version == 8:
                        data += struct.pack("<QI", int(refused), int(refused))
                        if refused:
                            data += field + struct.pack("<Q", 1)
                        data += struct.pack("<I", 0)
                    index.write_bytes(data)
                    resolve_no_vocabulary_band(index, [])
                    index.write_bytes(data + b"trailing")
                    with self.assertRaisesRegex(ValueError, "trailing bytes"):
                        resolve_no_vocabulary_band(index, [])
                    if version == 8:
                        index.write_bytes(data[:-1])
                        with self.assertRaises(ValueError):
                            resolve_no_vocabulary_band(index, [])


if __name__ == "__main__":
    unittest.main()
