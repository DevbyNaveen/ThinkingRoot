# Rooting Ablation on LongMemEval-500 — 2026-04-24

Two full 500-question LongMemEval-S runs against the same compiled
workspace (`longmemeval-workspace/`, 95,584 claims, 990 sources,
Azure gpt-4.1-mini for both synthesis and judging), toggling only
whether Rooting filters Rejected-tier claims out of retrieval.

Harness: `scripts/b2_ablation_run.sh`
Raw logs: `/Users/naveen/Desktop/thinkingroot/benchmarks/ablation/2026-04-24-gpt-5.4/off.log`, `/Users/naveen/Desktop/thinkingroot/benchmarks/ablation/2026-04-24-gpt-5.4/on.log`

## Headline

| Mode | Overall | Wall clock |
|------|---------|------------|
| off (retrieval sees all 95,584 claims) | Overall: 465/500 = 93.0% | 2834s |
| on  (retrieval excludes 1,210 Rejected claims) | Overall: 463/500 = 92.6% | 2825s |

## Per-category breakdown

### Mode=off (baseline)
```
                knowledge-update   77/78     98.7%
                   multi-session  117/133    88.0%
        single-session-assistant   55/56     98.2%
       single-session-preference   30/30    100.0%
             single-session-user   68/70     97.1%
              temporal-reasoning  118/133    88.7%
```

### Mode=on (Rooting filter)
```
                knowledge-update   77/78     98.7%
                   multi-session  121/133    91.0%
        single-session-assistant   54/56     96.4%
       single-session-preference   29/30     96.7%
             single-session-user   66/70     94.3%
              temporal-reasoning  116/133    87.2%
```

## Interpretation

This workspace was compiled before predicate extraction was wired into
the LLM prompts, so the only claims Rooting can reject are those
flagged by the Contradiction probe (1,210 of 95,584, 1.27%).
The ablation therefore measures a narrow but specific question: does
removing those flagged-contradictory claims change end-to-end
LongMemEval accuracy?

An improvement or no-regression result in the on-mode column
demonstrates that Rooting's fatal-probe rejections are at worst benign
to read-time accuracy, at best a positive signal. A regression would
indicate the gate is dropping load-bearing claims, which would warrant
tightening the contradiction-probe confidence floor
(`contradiction_floor`, default 0.85).

## Reproduction

```
cargo build --release -p thinkingroot-cli --bin root
AZURE_OPENAI_API_KEY=<key> ./scripts/b2_ablation_run.sh
```
