import unittest

from run_query_instruction_ab import (
    CODE_SEARCH_TASK,
    MODEL_CARD_TASK,
    aggregate_dense,
    arm_definitions,
    content_tokens,
    query_text,
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


if __name__ == "__main__":
    unittest.main()
