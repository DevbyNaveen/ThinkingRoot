#!/usr/bin/env python3
"""Prepare a LongMemEval workspace from the 101-question subset.

- Dedupes sessions across all questions by session_id.
- Writes each session as ws/sessions/{session_id}.md (date header + speaker-labeled
  turns, blank-line separated → prose chunks → witness-mesh atomic facts).
- Emits ws/subset.jsonl (one question per line) for `root eval`.
"""
import json, os, sys

SUBSET = "/Users/naveen/Desktop/thinkingroot/longmemeval-data/longmemeval_subset100.json"
WS = "/tmp/lme-eval"
SESS = os.path.join(WS, "sessions")
os.makedirs(SESS, exist_ok=True)

data = json.load(open(SUBSET))

sessions = {}   # session_id -> (date, turns)
for q in data:
    ids = q["haystack_session_ids"]
    dates = q.get("haystack_dates", [])
    sess = q["haystack_sessions"]
    for i, sid in enumerate(ids):
        if sid in sessions:
            continue
        date = dates[i] if i < len(dates) else ""
        sessions[sid] = (date, sess[i])

written = 0
for sid, (date, turns) in sessions.items():
    lines = []
    # A fact-shaped date sentence so the temporal extractor can anchor it.
    if date:
        lines.append(f"This conversation took place on {date}.")
        lines.append("")
    for t in turns:
        role = "User" if t.get("role") == "user" else "Assistant"
        content = (t.get("content") or "").strip()
        if not content:
            continue
        lines.append(f"{role}: {content}")
        lines.append("")
    with open(os.path.join(SESS, f"{sid}.md"), "w") as f:
        f.write("\n".join(lines))
    written += 1

with open(os.path.join(WS, "subset.jsonl"), "w") as f:
    for q in data:
        f.write(json.dumps(q) + "\n")

print(f"sessions written: {written}")
print(f"questions (jsonl): {len(data)}")
print(f"workspace: {WS}")
