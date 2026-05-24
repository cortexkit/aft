"""Retrieval metrics for AFT search benchmarks.

The Vera-compatible formulas in this file are attributed to Vera's evaluation
harness (`Vera/eval/src/metrics.rs`) and reimplemented from scratch for AFT.
Vera's task corpus uses line-range ground truth; this module keeps relevance
pluggable so the older AFT file-only fixtures can continue to use path matches.
"""

from __future__ import annotations

import math
from typing import Any, Callable, Dict, Iterable, List, Optional, Sequence


JsonObject = Dict[str, Any]
RelevanceFn = Callable[[JsonObject, JsonObject], bool]


def normalize_path(value: Any) -> str:
    return str(value or "").replace("\\", "/").lstrip("./")


def _int_or_none(value: Any) -> Optional[int]:
    if isinstance(value, bool):
        return None
    if isinstance(value, int):
        return value
    if isinstance(value, float) and value.is_integer():
        return int(value)
    if isinstance(value, str):
        try:
            return int(value)
        except ValueError:
            return None
    return None


def prediction_path(prediction: JsonObject) -> str:
    return normalize_path(prediction.get("file_path", prediction.get("file", "")))


def ground_truth_path(ground_truth: JsonObject) -> str:
    return normalize_path(ground_truth.get("file_path", ground_truth.get("file", "")))


def prediction_start_line(prediction: JsonObject) -> Optional[int]:
    return _int_or_none(prediction.get("line_start", prediction.get("start_line")))


def prediction_end_line(prediction: JsonObject) -> Optional[int]:
    return _int_or_none(prediction.get("line_end", prediction.get("end_line")))


def ground_truth_start_line(ground_truth: JsonObject) -> Optional[int]:
    return _int_or_none(ground_truth.get("line_start", ground_truth.get("start_line")))


def ground_truth_end_line(ground_truth: JsonObject) -> Optional[int]:
    return _int_or_none(ground_truth.get("line_end", ground_truth.get("end_line")))


def line_ranges_overlap(prediction: JsonObject, ground_truth: JsonObject) -> bool:
    pred_start = prediction_start_line(prediction)
    pred_end = prediction_end_line(prediction)
    truth_start = ground_truth_start_line(ground_truth)
    truth_end = ground_truth_end_line(ground_truth)
    if pred_start is None or pred_end is None or truth_start is None or truth_end is None:
        return False
    return max(pred_start, truth_start) <= min(pred_end, truth_end)


def line_overlap_relevance(prediction: JsonObject, ground_truth: JsonObject) -> bool:
    return prediction_path(prediction) == ground_truth_path(ground_truth) and line_ranges_overlap(
        prediction, ground_truth
    )


def file_path_relevance(prediction: JsonObject, ground_truth: JsonObject) -> bool:
    return prediction_path(prediction) == ground_truth_path(ground_truth)


def relevance_for_mode(mode: str) -> RelevanceFn:
    if mode == "line_overlap":
        return line_overlap_relevance
    if mode == "file_path":
        return file_path_relevance
    raise ValueError(f"unknown relevance mode: {mode}")


def is_relevant(prediction: JsonObject, ground_truth: Sequence[JsonObject], relevance_fn: RelevanceFn) -> bool:
    return any(relevance_fn(prediction, truth) for truth in ground_truth)


def best_matching_truth_index(
    prediction: JsonObject,
    ground_truth: Sequence[JsonObject],
    relevance_fn: RelevanceFn,
    used_truth_indexes: Optional[set[int]] = None,
) -> Optional[int]:
    best_index: Optional[int] = None
    best_gain = -1.0
    used_truth_indexes = used_truth_indexes or set()
    for index, truth in enumerate(ground_truth):
        if index in used_truth_indexes or not relevance_fn(prediction, truth):
            continue
        gain = float(truth.get("relevance", 1.0))
        if gain > best_gain:
            best_gain = gain
            best_index = index
    return best_index


def matching_relevance(prediction: JsonObject, ground_truth: Sequence[JsonObject], relevance_fn: RelevanceFn) -> float:
    index = best_matching_truth_index(prediction, ground_truth, relevance_fn)
    if index is None:
        return 0.0
    return float(ground_truth[index].get("relevance", 1.0))


def covered_truth_indexes(
    predictions: Sequence[JsonObject], ground_truth: Sequence[JsonObject], k: int, relevance_fn: RelevanceFn
) -> List[int]:
    covered = set()
    for prediction in predictions[:k]:
        for index, truth in enumerate(ground_truth):
            if relevance_fn(prediction, truth):
                covered.add(index)
    return sorted(covered)


def recall_at_k(
    predictions: Sequence[JsonObject], ground_truth: Sequence[JsonObject], k: int, relevance_fn: RelevanceFn = line_overlap_relevance
) -> float:
    if not ground_truth:
        return 0.0
    return len(covered_truth_indexes(predictions, ground_truth, k, relevance_fn)) / len(ground_truth)


def precision_at_k(
    predictions: Sequence[JsonObject], ground_truth: Sequence[JsonObject], k: int, relevance_fn: RelevanceFn = line_overlap_relevance
) -> float:
    if k <= 0:
        return 0.0
    relevant = sum(1 for prediction in predictions[:k] if is_relevant(prediction, ground_truth, relevance_fn))
    return relevant / k


def mrr_at_k(
    predictions: Sequence[JsonObject], ground_truth: Sequence[JsonObject], k: int, relevance_fn: RelevanceFn = line_overlap_relevance
) -> float:
    for rank, prediction in enumerate(predictions[:k], start=1):
        if is_relevant(prediction, ground_truth, relevance_fn):
            return 1.0 / rank
    return 0.0


def dcg_at_k(
    predictions: Sequence[JsonObject], ground_truth: Sequence[JsonObject], k: int, relevance_fn: RelevanceFn
) -> float:
    dcg = 0.0
    used_truth_indexes: set[int] = set()
    for rank, prediction in enumerate(predictions[:k], start=1):
        truth_index = best_matching_truth_index(prediction, ground_truth, relevance_fn, used_truth_indexes)
        if truth_index is None:
            continue
        used_truth_indexes.add(truth_index)
        gain = float(ground_truth[truth_index].get("relevance", 1.0))
        dcg += gain / math.log2(rank + 1)
    return dcg


def ndcg_at_k(
    predictions: Sequence[JsonObject], ground_truth: Sequence[JsonObject], k: int, relevance_fn: RelevanceFn = line_overlap_relevance
) -> float:
    if not ground_truth or k <= 0:
        return 0.0
    dcg = dcg_at_k(predictions, ground_truth, k, relevance_fn)
    ideal_gains = sorted((float(truth.get("relevance", 1.0)) for truth in ground_truth), reverse=True)[:k]
    ideal_dcg = sum(gain / math.log2(rank + 1) for rank, gain in enumerate(ideal_gains, start=1))
    if ideal_dcg == 0.0:
        return 0.0
    return dcg / ideal_dcg


def round_metric(value: float) -> float:
    return round(float(value), 6)


def evaluate_retrieval(
    predictions: Sequence[JsonObject], ground_truth: Sequence[JsonObject], relevance_fn: RelevanceFn = line_overlap_relevance
) -> JsonObject:
    return {
        "precision_at_1": round_metric(precision_at_k(predictions, ground_truth, 1, relevance_fn)),
        "precision_at_5": round_metric(precision_at_k(predictions, ground_truth, 5, relevance_fn)),
        "precision_at_10": round_metric(precision_at_k(predictions, ground_truth, 10, relevance_fn)),
        "recall_at_1": round_metric(recall_at_k(predictions, ground_truth, 1, relevance_fn)),
        "recall_at_5": round_metric(recall_at_k(predictions, ground_truth, 5, relevance_fn)),
        "recall_at_10": round_metric(recall_at_k(predictions, ground_truth, 10, relevance_fn)),
        "mrr": round_metric(mrr_at_k(predictions, ground_truth, 10, relevance_fn)),
        "ndcg_at_10": round_metric(ndcg_at_k(predictions, ground_truth, 10, relevance_fn)),
    }


def average_dicts(items: Iterable[JsonObject], keys: Sequence[str]) -> JsonObject:
    values = list(items)
    if not values:
        return {key: 0.0 for key in keys}
    return {key: round_metric(sum(float(item.get(key, 0.0)) for item in values) / len(values)) for key in keys}
