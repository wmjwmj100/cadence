#!/usr/bin/env python3
"""Deterministic offline profile-growth evaluation harness.

This script intentionally avoids LLM calls. It evaluates whether a profile
update policy can grow a durable user profile across turns, close stale facts,
preserve provenance, and retrieve a small task-relevant profile view.
"""

from __future__ import annotations

import json
import re
import sys
from dataclasses import dataclass, field
from typing import Iterable


@dataclass(frozen=True)
class Turn:
    turn_id: str
    text: str


@dataclass(frozen=True)
class Case:
    case_id: str
    turns: tuple[Turn, ...]
    expected_active: frozenset[str]
    expected_inactive: frozenset[str]
    retrieval_query: str
    expected_retrieved: frozenset[str]
    note: str


@dataclass
class FactRecord:
    key: str
    evidence_turn_ids: list[str]
    status: str = "active"


@dataclass
class ProfileState:
    facts: dict[str, FactRecord] = field(default_factory=dict)

    def add_or_refresh(self, key: str, turn_id: str) -> None:
        record = self.facts.get(key)
        if record is None:
            self.facts[key] = FactRecord(key=key, evidence_turn_ids=[turn_id])
            return
        if turn_id not in record.evidence_turn_ids:
            record.evidence_turn_ids.append(turn_id)
        record.status = "active"

    def deactivate(self, key: str, turn_id: str) -> None:
        record = self.facts.get(key)
        if record is None:
            return
        if turn_id not in record.evidence_turn_ids:
            record.evidence_turn_ids.append(turn_id)
        record.status = "inactive"

    def active_keys(self) -> set[str]:
        return {key for key, record in self.facts.items() if record.status == "active"}

    def inactive_keys(self) -> set[str]:
        return {key for key, record in self.facts.items() if record.status == "inactive"}


FACT_PATTERNS: tuple[tuple[str, str], ...] = (
    (r"\bvegetarian\b", "diet:vegetarian"),
    (r"\bdairy[- ]free\b|\bavoid dairy\b", "diet:dairy_free"),
    (r"\bnot vegetarian anymore\b|\bno longer vegetarian\b", "correction:not_vegetarian"),
    (r"\bprefer python\b|\buse python\b", "tool:python"),
    (r"\brust\b", "tool:rust"),
    (r"\bconcise\b|\bbrief\b", "style:concise"),
    (r"\bdetailed\b|\bmore detail\b", "style:detailed"),
    (r"\bweekly on fridays\b|\bevery friday\b", "schedule:weekly_friday"),
    (r"\btokyo\b", "trip:tokyo"),
    (r"\bkyoto\b", "trip:kyoto"),
    (r"\bbudget\b|\bcheap\b", "travel:budget"),
    (r"\baccessibility\b|\bwheelchair\b", "accessibility:wheelchair"),
    (r"\bproject atlas\b", "project:atlas"),
    (r"\bgraphql\b", "project_atlas:graphql"),
    (r"\bpostgres\b", "project_atlas:postgres"),
    (r"\bno longer using postgres\b|\bmoved from postgres to sqlite\b", "correction:no_postgres"),
    (r"\bsqlite\b", "project_atlas:sqlite"),
    (r"\bdon't remember this\b|\bdo not remember this\b|\bno memory\b", "control:no_memory"),
)

CONTRADICTIONS: dict[str, tuple[str, ...]] = {
    "correction:not_vegetarian": ("diet:vegetarian",),
    "style:detailed": ("style:concise",),
    "style:concise": ("style:detailed",),
    "correction:no_postgres": ("project_atlas:postgres",),
}

CONTROL_KEYS = {"control:no_memory"}
CORRECTION_KEYS = {key for key in CONTRADICTIONS if key.startswith("correction:")}

RETRIEVAL_TERMS: dict[str, tuple[str, ...]] = {
    "diet:vegetarian": ("meal", "food", "restaurant", "dinner", "diet", "vegetarian"),
    "diet:dairy_free": ("meal", "food", "restaurant", "dinner", "diet", "dairy"),
    "tool:python": ("code", "script", "tool", "python", "implementation"),
    "tool:rust": ("code", "script", "tool", "rust", "implementation"),
    "style:concise": ("reply", "write", "style", "format", "answer"),
    "style:detailed": ("reply", "write", "style", "format", "answer"),
    "schedule:weekly_friday": ("schedule", "reminder", "meeting", "calendar", "weekly"),
    "trip:tokyo": ("trip", "travel", "itinerary", "japan", "tokyo"),
    "trip:kyoto": ("trip", "travel", "itinerary", "japan", "kyoto"),
    "travel:budget": ("trip", "travel", "itinerary", "budget", "cheap"),
    "accessibility:wheelchair": ("trip", "travel", "itinerary", "accessibility", "wheelchair"),
    "project:atlas": ("project", "atlas", "architecture", "work"),
    "project_atlas:graphql": ("project", "atlas", "architecture", "api", "graphql"),
    "project_atlas:postgres": ("project", "atlas", "architecture", "database", "postgres"),
    "project_atlas:sqlite": ("project", "atlas", "architecture", "database", "sqlite"),
}

CASES: tuple[Case, ...] = (
    Case(
        case_id="diet_correction",
        turns=(
            Turn("diet_t1", "I am vegetarian and avoid dairy."),
            Turn("diet_t2", "For dinner ideas, keep them quick."),
            Turn("diet_t3", "Update: I am not vegetarian anymore, but I still avoid dairy."),
        ),
        expected_active=frozenset({"diet:dairy_free"}),
        expected_inactive=frozenset({"diet:vegetarian"}),
        retrieval_query="Suggest a restaurant for dinner",
        expected_retrieved=frozenset({"diet:dairy_free"}),
        note="Temporal correction should close stale diet fact while retaining compatible constraint.",
    ),
    Case(
        case_id="tool_and_style_drift",
        turns=(
            Turn("tool_t1", "For code examples I prefer Python and concise explanations."),
            Turn("tool_t2", "Actually, for this Rust project I need more detailed answers."),
            Turn("tool_t3", "Still use Python for quick scripts, but Rust for production code."),
        ),
        expected_active=frozenset({"tool:python", "tool:rust", "style:detailed"}),
        expected_inactive=frozenset({"style:concise"}),
        retrieval_query="Write implementation guidance for code",
        expected_retrieved=frozenset({"tool:python", "tool:rust"}),
        note="Compatible tool preferences should accumulate; style correction should replace old style.",
    ),
    Case(
        case_id="travel_constraints",
        turns=(
            Turn("travel_t1", "I am planning a Tokyo and Kyoto trip on a budget."),
            Turn("travel_t2", "Accessibility matters because my partner uses a wheelchair."),
            Turn("travel_t3", "Please remember this for itinerary planning."),
        ),
        expected_active=frozenset({"trip:tokyo", "trip:kyoto", "travel:budget", "accessibility:wheelchair"}),
        expected_inactive=frozenset(),
        retrieval_query="Make a Japan travel itinerary",
        expected_retrieved=frozenset({"trip:tokyo", "trip:kyoto", "travel:budget", "accessibility:wheelchair"}),
        note="Multi-turn profile growth should combine destination and accessibility constraints.",
    ),
    Case(
        case_id="project_database_change",
        turns=(
            Turn("project_t1", "Project Atlas uses GraphQL with Postgres."),
            Turn("project_t2", "We moved from Postgres to SQLite for Project Atlas."),
            Turn("project_t3", "Please keep replies brief when summarizing the architecture."),
        ),
        expected_active=frozenset({"project:atlas", "project_atlas:graphql", "project_atlas:sqlite", "style:concise"}),
        expected_inactive=frozenset({"project_atlas:postgres"}),
        retrieval_query="Summarize Project Atlas database architecture",
        expected_retrieved=frozenset({"project:atlas", "project_atlas:graphql", "project_atlas:sqlite"}),
        note="Project facts should persist while an obsolete database fact is closed.",
    ),
)


def extract_candidates(text: str) -> list[str]:
    lower_text = text.lower()
    candidates = [key for pattern, key in FACT_PATTERNS if re.search(pattern, lower_text)]
    if "moved from postgres to sqlite" in lower_text and "project_atlas:postgres" in candidates:
        candidates.remove("project_atlas:postgres")
    if "not vegetarian anymore" in lower_text and "diet:vegetarian" in candidates:
        candidates.remove("diet:vegetarian")
    return candidates


def run_append_only(case: Case) -> ProfileState:
    state = ProfileState()
    for turn in case.turns:
        for candidate in extract_candidates(turn.text):
            if candidate not in CONTROL_KEYS and candidate not in CORRECTION_KEYS:
                state.add_or_refresh(candidate, turn.turn_id)
    return state


def run_last_turn_only(case: Case) -> ProfileState:
    state = ProfileState()
    final_turn = case.turns[-1]
    for candidate in extract_candidates(final_turn.text):
        if candidate not in CONTROL_KEYS and candidate not in CORRECTION_KEYS:
            state.add_or_refresh(candidate, final_turn.turn_id)
    return state


def run_evidence_atomic(case: Case) -> ProfileState:
    state = ProfileState()
    for turn in case.turns:
        candidates = extract_candidates(turn.text)
        if any(candidate in CONTROL_KEYS for candidate in candidates):
            continue
        for candidate in candidates:
            for stale_key in CONTRADICTIONS.get(candidate, ()):
                state.deactivate(stale_key, turn.turn_id)
            if candidate in CORRECTION_KEYS:
                continue
            state.add_or_refresh(candidate, turn.turn_id)
    return state


def safe_divide(numerator: float, denominator: float) -> float:
    if denominator == 0:
        return 1.0 if numerator == 0 else 0.0
    return numerator / denominator


def f1(precision: float, recall: float) -> float:
    if precision + recall == 0:
        return 0.0
    return 2 * precision * recall / (precision + recall)


def retrieve(active_keys: Iterable[str], query: str, limit: int = 4) -> list[str]:
    query_terms = set(re.findall(r"[a-z0-9]+", query.lower()))
    scored: list[tuple[int, str]] = []
    for key in active_keys:
        terms = set(RETRIEVAL_TERMS.get(key, ()))
        score = len(query_terms & terms)
        if score:
            scored.append((score, key))
    scored.sort(key=lambda item: (-item[0], item[1]))
    return [key for _, key in scored[:limit]]


def evaluate_policy(policy_name: str, cases: tuple[Case, ...]) -> dict[str, float]:
    runner = {
        "append_only": run_append_only,
        "last_turn_only": run_last_turn_only,
        "evidence_atomic": run_evidence_atomic,
    }[policy_name]

    active_true_positive = 0
    active_predicted = 0
    active_expected = 0
    inactive_true_positive = 0
    inactive_expected = 0
    active_with_provenance = 0
    retrieval_true_positive = 0
    retrieval_predicted = 0
    retrieval_expected = 0
    total_active_facts = 0

    per_case: dict[str, dict[str, object]] = {}

    for case in cases:
        state = runner(case)
        active = state.active_keys()
        inactive = state.inactive_keys()
        retrieved = set(retrieve(active, case.retrieval_query))

        active_true_positive += len(active & case.expected_active)
        active_predicted += len(active)
        active_expected += len(case.expected_active)
        inactive_true_positive += len(inactive & case.expected_inactive)
        inactive_expected += len(case.expected_inactive)
        active_with_provenance += sum(
            1 for key in active if state.facts[key].evidence_turn_ids
        )
        retrieval_true_positive += len(retrieved & case.expected_retrieved)
        retrieval_predicted += len(retrieved)
        retrieval_expected += len(case.expected_retrieved)
        total_active_facts += len(active)

        per_case[case.case_id] = {
            "active": sorted(active),
            "inactive": sorted(inactive),
            "retrieved": sorted(retrieved),
        }

    active_precision = safe_divide(active_true_positive, active_predicted)
    active_recall = safe_divide(active_true_positive, active_expected)
    retrieval_precision = safe_divide(retrieval_true_positive, retrieval_predicted)
    retrieval_recall = safe_divide(retrieval_true_positive, retrieval_expected)

    return {
        "active_precision": round(active_precision, 4),
        "active_recall": round(active_recall, 4),
        "active_f1": round(f1(active_precision, active_recall), 4),
        "inactive_recall": round(safe_divide(inactive_true_positive, inactive_expected), 4),
        "provenance_coverage": round(safe_divide(active_with_provenance, active_predicted), 4),
        "retrieval_precision": round(retrieval_precision, 4),
        "retrieval_recall": round(retrieval_recall, 4),
        "avg_active_facts": round(total_active_facts / len(cases), 4),
        "per_case": per_case,
    }


def main() -> int:
    results = {
        policy_name: evaluate_policy(policy_name, CASES)
        for policy_name in ("append_only", "last_turn_only", "evidence_atomic")
    }
    print(json.dumps(results, indent=2, sort_keys=True))

    proposed = results["evidence_atomic"]
    thresholds = {
        "active_precision": 0.80,
        "active_recall": 0.80,
        "active_f1": 0.80,
        "inactive_recall": 0.80,
        "provenance_coverage": 1.00,
        "retrieval_precision": 0.80,
        "retrieval_recall": 0.80,
    }
    failures = {
        metric: {"actual": proposed[metric], "required": required}
        for metric, required in thresholds.items()
        if proposed[metric] < required
    }
    if failures:
        print(json.dumps({"threshold_failures": failures}, indent=2, sort_keys=True), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
