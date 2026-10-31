from copy import deepcopy
import unittest

from compare_engine_only_profile import response_bytes


class ResponseByteComparisonTest(unittest.TestCase):
    def setUp(self):
        self.response = {
            "structuredContent": {"plan": {"snapshot_generation": "2:3_" + "a" * 32}},
            "results": [{"file": "src/a.rs", "line": 12, "snippet": "a\nb"}],
            "results_list_envelope": {"shown": 1, "total": {"kind": "exact", "value": 1}},
        }

    def test_only_process_nonce_is_ignored(self):
        other = deepcopy(self.response)
        other["structuredContent"]["plan"]["snapshot_generation"] = "2:3_" + "b" * 32
        self.assertEqual(response_bytes(self.response), response_bytes(other))
        other["structuredContent"]["plan"]["snapshot_generation"] = "2:4_" + "b" * 32
        self.assertNotEqual(response_bytes(self.response), response_bytes(other))

    def test_result_and_envelope_bytes_are_not_ignored(self):
        for key, value in [("file", "src/b.rs"), ("line", 13), ("snippet", "a b")]:
            other = deepcopy(self.response)
            other["results"][0][key] = value
            self.assertNotEqual(response_bytes(self.response), response_bytes(other))
        other = deepcopy(self.response)
        other["results_list_envelope"]["total"]["value"] = 2
        self.assertNotEqual(response_bytes(self.response), response_bytes(other))

    def test_unrecognized_nonce_is_rejected(self):
        self.response["structuredContent"]["plan"]["snapshot_generation"] = "2:3_not-a-nonce"
        with self.assertRaises(AssertionError):
            response_bytes(self.response)


if __name__ == "__main__":
    unittest.main()
