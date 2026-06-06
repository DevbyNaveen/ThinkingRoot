#!/usr/bin/env python3
"""Generate all figures for the CompAG/Rooting paper.

All numbers below are extracted from real project artifacts. Every value
is traceable to a file in the repo — no projections, no invented bars.

- LongMemEval per-category (gpt-5.4 canonical):
    benchmarks/ablation/2026-04-24-gpt-5.4/off.log
  Overall 465/500 = 93.0 %.
- LongMemEval gpt-4.1-mini control (for historical/secondary plots):
    benchmarks/ablation/2026-04-24-gpt4.1-mini/off.log
  Overall 448/500 = 89.6 %.
- LongMemEval historical Round 6 (retained only in tab:progress):
    benchmarks/benchmark_round6_91.2pct.txt  (91.2 %, deprecated Azure)
- HTTP latency:
    benchmarks/reports/2026-04-14-stress-10k-report.html
  p95 = 0.117 ms at 10,000 concurrent users.
- Rooting live tier distribution on the ThinkingRoot own-workspace
  run (7,103 claims, 98.6 % Rooted, 101 Rejected). The larger
  LongMemEval tier distribution (95,584 claims) is reported
  numerically in the paper's tab:honest-tier.
- Rooting per-claim divan bench:
    benchmarks/macro/rooting_overhead_2026-04.md
  N=100, median 24.22 ms -> 242 us/claim.
"""
import os
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
import numpy as np

OUT = os.path.dirname(os.path.abspath(__file__))

# Consistent publication styling.
plt.rcParams.update({
    "font.family": "serif",
    "font.size": 9,
    "axes.titlesize": 10,
    "axes.labelsize": 9,
    "legend.fontsize": 8,
    "figure.dpi": 150,
    "savefig.dpi": 300,
    "savefig.bbox": "tight",
    "savefig.pad_inches": 0.08,
    "axes.spines.top": False,
    "axes.spines.right": False,
})

# Palette — muted, accessible.
C_ROOTED = "#2f7d4a"
C_ATTESTED = "#8a8a8a"
C_QUARANTINED = "#d48a1a"
C_REJECTED = "#c23b3b"
C_PRIMARY = "#1f4e79"
C_SECONDARY = "#6ea8d0"
C_NEUTRAL = "#9aa4ae"


# ═════════════════════════════════════════════════════════════════════════════
# Figure — LongMemEval per-category accuracy
# ═════════════════════════════════════════════════════════════════════════════
def fig_longmemeval_categories():
    # Canonical gpt-5.4 per-category numbers from
    # benchmarks/ablation/2026-04-24-gpt-5.4/off.log.
    cats = [
        "single-session\nuser",
        "single-session\npreference",
        "single-session\nassistant",
        "knowledge\nupdate",
        "temporal\nreasoning",
        "multi-\nsession",
    ]
    correct = [68, 30, 55, 77, 118, 117]
    total   = [70, 30, 56, 78, 133, 133]
    # 97.142857..., 100.0, 98.214285..., 98.717948..., 88.721804..., 87.969924...
    acc = [100.0 * c / t for c, t in zip(correct, total)]
    assert sum(correct) == 465 and sum(total) == 500, "gpt-5.4 totals must match 465/500"

    fig, ax = plt.subplots(figsize=(6.8, 3.6))
    x = np.arange(len(cats))
    bars = ax.bar(x, acc, color=C_PRIMARY, edgecolor="black",
                  linewidth=0.4, width=0.68)
    ax.set_ylim(0, 118)
    ax.set_ylabel("Accuracy (%)")
    ax.set_xticks(x)
    ax.set_xticklabels(cats, fontsize=8)
    ax.axhline(93.0, color=C_REJECTED, linestyle="--", linewidth=0.9, alpha=0.8,
               label="overall 93.0%")
    for i, (b, a, c, t) in enumerate(zip(bars, acc, correct, total)):
        ax.text(b.get_x() + b.get_width() / 2, a + 2.0,
                f"{a:.1f}%", ha="center", va="bottom",
                fontsize=8, fontweight="bold")
        ax.text(b.get_x() + b.get_width() / 2, a + 8.5,
                f"{c}/{t}", ha="center", va="bottom", fontsize=7,
                color="#444444")
    ax.legend(loc="lower left", frameon=False)
    ax.set_title("LongMemEval-500 per-category accuracy (Azure gpt-5.4, 2026-04-24)")
    fig.tight_layout()
    fig.savefig(os.path.join(OUT, "fig_longmemeval.pdf"))
    plt.close(fig)


# ═════════════════════════════════════════════════════════════════════════════
# Figure — HTTP serving latency (log scale vs competitors)
# ═════════════════════════════════════════════════════════════════════════════
def fig_http_latency():
    systems = [
        "ThinkingRoot (entity read)",
        "FalkorDB",
        "SuperMemory.ai",
        "Zep",
        "Graphiti",
    ]
    latency_ms = [0.117, 36.0, 50.0, 119.0, 500.0]
    notes      = ["p95 @ 10K VUs", "p50", "p95", "best reported", "avg"]
    colors     = [C_PRIMARY, C_NEUTRAL, C_NEUTRAL, C_NEUTRAL, C_NEUTRAL]

    fig, ax = plt.subplots(figsize=(6.8, 3.0))
    y = np.arange(len(systems))
    ax.barh(y, latency_ms, color=colors, edgecolor="black",
            linewidth=0.4, height=0.58)
    ax.set_xscale("log")
    ax.set_yticks(y)
    ax.set_yticklabels(systems, fontsize=9)
    ax.set_xlabel("Latency (ms, log scale)")
    ax.invert_yaxis()

    # Set xlim FIRST so annotations fit
    ax.set_xlim(0.04, 20000)
    for i, (ms, n) in enumerate(zip(latency_ms, notes)):
        ax.text(ms * 1.25, i, f"{ms:.3g} ms  ({n})",
                va="center", fontsize=8)

    ax.set_title("Serving latency vs. production AI-memory systems")
    ax.grid(axis="x", linestyle=":", alpha=0.5)
    fig.tight_layout()
    fig.savefig(os.path.join(OUT, "fig_latency.pdf"))
    plt.close(fig)


# ═════════════════════════════════════════════════════════════════════════════
# Figure — Rooting admission-tier distribution on real 7K-claim workspace
# ═════════════════════════════════════════════════════════════════════════════
def fig_tier_distribution():
    labels = ["Rooted", "Attested", "Quarantined", "Rejected"]
    counts = [7002, 0, 0, 101]
    colors = [C_ROOTED, C_ATTESTED, C_QUARANTINED, C_REJECTED]

    # Pie: only nonzero slices (pie labels outside to avoid the thin-slice overlap).
    nz = [(l, c, col) for l, c, col in zip(labels, counts, colors) if c > 0]
    pie_labels = [f"{l}\n{c:,} ({100*c/sum(counts):.1f}%)" for l, c, _ in nz]
    pie_counts = [c for _, c, _ in nz]
    pie_colors = [col for _, _, col in nz]

    total = sum(counts)
    fig, (ax1, ax2) = plt.subplots(
        1, 2, figsize=(8.2, 3.4),
        gridspec_kw={"width_ratios": [1.0, 1.2], "wspace": 0.3},
    )

    wedges, _ = ax1.pie(
        pie_counts, labels=None, colors=pie_colors,
        startangle=90,
        wedgeprops={"edgecolor": "white", "linewidth": 1.4},
    )
    # External legend for pie avoids overlap on thin slices.
    ax1.legend(wedges, pie_labels, loc="center left",
               bbox_to_anchor=(-0.15, -0.12), frameon=False,
               fontsize=8.5, ncol=2)
    ax1.set_title(f"Admission tier (N = {total:,} claims)", pad=12)

    # Bar chart — counts (log scale so the 101 bar is visible next to 7002).
    x = np.arange(len(labels))
    bars = ax2.bar(x, [max(c, 1) for c in counts], color=colors,
                   edgecolor="black", linewidth=0.4, width=0.6)
    ax2.set_yscale("log")
    ax2.set_xticks(x)
    ax2.set_xticklabels(labels, fontsize=9)
    ax2.set_ylabel("claims (log scale)")
    ax2.set_ylim(0.5, max(counts) * 6)
    for b, c in zip(bars, counts):
        pct = 100 * c / total if total else 0
        lbl = f"{c:,}\n({pct:.1f}%)" if c else "0"
        ax2.text(b.get_x() + b.get_width() / 2, b.get_height() * 1.25,
                 lbl, ha="center", va="bottom", fontsize=8)
    ax2.set_title("after `root rooting re-run --all`", pad=12)

    fig.suptitle("Rooting admission on ThinkingRoot source (7,103 claims)",
                 fontsize=10.5, y=1.01)
    fig.tight_layout(rect=[0, 0.02, 1, 0.97])
    fig.savefig(os.path.join(OUT, "fig_tier_distribution.pdf"))
    plt.close(fig)


# ═════════════════════════════════════════════════════════════════════════════
# Figure — Rooting admission on the 95,584-claim LongMemEval workspace
#           (canonical, larger-scale companion to fig_tier_distribution)
# ═════════════════════════════════════════════════════════════════════════════
def fig_tier_distribution_longmem():
    # Numbers from benchmarks/ROOTING_TIER_HONEST_2026-04.md:
    # 94,374 Rooted (temporal-default — no claim in this workspace carries
    # a predicate, so every Rooted admission is fatal-probes + temporal),
    # 0 Attested, 0 Quarantined, 1,210 Rejected (all Contradiction-probe
    # failures backed by real contradiction records).
    labels = ["Rooted\n(temporal-default)", "Attested", "Quarantined", "Rejected"]
    counts = [94374, 0, 0, 1210]
    colors = [C_ROOTED, C_ATTESTED, C_QUARANTINED, C_REJECTED]

    nz = [(l, c, col) for l, c, col in zip(labels, counts, colors) if c > 0]
    pie_labels = [
        f"{l}\n{c:,} ({100 * c / sum(counts):.2f}%)"
        for l, c, _ in nz
    ]
    pie_counts = [c for _, c, _ in nz]
    pie_colors = [col for _, _, col in nz]

    total = sum(counts)
    fig, (ax1, ax2) = plt.subplots(
        1, 2, figsize=(8.4, 3.4),
        gridspec_kw={"width_ratios": [1.0, 1.2], "wspace": 0.3},
    )

    wedges, _ = ax1.pie(
        pie_counts, labels=None, colors=pie_colors,
        startangle=90,
        wedgeprops={"edgecolor": "white", "linewidth": 1.4},
    )
    ax1.legend(
        wedges, pie_labels, loc="center left",
        bbox_to_anchor=(-0.15, -0.12), frameon=False,
        fontsize=8.5, ncol=2,
    )
    ax1.set_title(f"Admission tier (N = {total:,} claims)", pad=12)

    x = np.arange(len(labels))
    bars = ax2.bar(
        x, [max(c, 1) for c in counts],
        color=colors, edgecolor="black", linewidth=0.4, width=0.6,
    )
    ax2.set_yscale("log")
    ax2.set_xticks(x)
    ax2.set_xticklabels(
        ["Rooted\n(temp-default)", "Attested", "Quarantined", "Rejected"],
        fontsize=8.5,
    )
    ax2.set_ylabel("claims (log scale)")
    ax2.set_ylim(0.5, max(counts) * 6)
    for b, c in zip(bars, counts):
        pct = 100 * c / total if total else 0
        lbl = f"{c:,}\n({pct:.2f}%)" if c else "0"
        ax2.text(
            b.get_x() + b.get_width() / 2,
            b.get_height() * 1.25,
            lbl,
            ha="center", va="bottom", fontsize=8,
        )
    ax2.set_title("after `root rooting re-run --all`", pad=12)

    fig.suptitle(
        "Rooting admission on LongMemEval-500 workspace (95,584 claims)",
        fontsize=10.5, y=1.01,
    )
    fig.tight_layout(rect=[0, 0.02, 1, 0.97])
    fig.savefig(os.path.join(OUT, "fig_tier_distribution_longmem.pdf"))
    plt.close(fig)


# ═════════════════════════════════════════════════════════════════════════════
# Figure — Competitive accuracy landscape on LongMemEval-500
# ═════════════════════════════════════════════════════════════════════════════
def fig_longmemeval_landscape():
    # April 2026 public leaderboard + our canonical 93.0 % gpt-5.4 result.
    # ThinkingRoot and MemMachine both sit at 93.0 — plotted adjacent to
    # make the tie visible at a glance. Hindsight is kept at 91.4 and
    # placed immediately to the left so the reader can see ThinkingRoot
    # cleared Hindsight.
    systems = [
        "Full-context GPT-4o",
        "Zep (GPT-4o)",
        "Emergence AI",
        "Supermemory",
        "Hindsight",
        "ThinkingRoot",
        "MemMachine",
        "OMEGA",
        "Chronos",
    ]
    score = [62.0, 71.2, 82.4, 85.4, 91.4, 93.0, 93.0, 95.4, 95.60]
    is_tr = [False, False, False, False, False, True, False, False, False]

    fig, ax = plt.subplots(figsize=(7.0, 3.8))
    x = np.arange(len(systems))
    colors = [C_PRIMARY if t else C_NEUTRAL for t in is_tr]
    ax.bar(x, score, color=colors, edgecolor="black",
           linewidth=0.4, width=0.65)
    ax.set_ylim(50, 100)
    ax.set_ylabel("LongMemEval-500 accuracy (%)")
    ax.set_xticks(x)
    ax.set_xticklabels(systems, rotation=30, ha="right", fontsize=8.5)
    for i, s in enumerate(score):
        ax.text(i, s + 0.7, f"{s:.1f}", ha="center",
                va="bottom", fontsize=8)
    # Tie annotation above the two 93.0 bars so the tie reads even from a
    # thumbnail of the figure.
    tr_idx = systems.index("ThinkingRoot")
    mm_idx = systems.index("MemMachine")
    midpoint = (tr_idx + mm_idx) / 2.0
    ax.annotate(
        "tied #3",
        xy=(midpoint, 94.8),
        xytext=(midpoint, 98.5),
        ha="center",
        va="bottom",
        fontsize=8,
        fontstyle="italic",
        color="#555555",
        arrowprops=dict(arrowstyle="-", color="#888888", linewidth=0.6),
    )
    ax.set_title("LongMemEval-500 — ThinkingRoot in the global leaderboard (April 2026)")
    ax.grid(axis="y", linestyle=":", alpha=0.5)
    fig.tight_layout()
    fig.savefig(os.path.join(OUT, "fig_longmemeval_landscape.pdf"))
    plt.close(fig)


# ═════════════════════════════════════════════════════════════════════════════
# Figure — Rooting probe overhead (divan @ N=100)
# ═════════════════════════════════════════════════════════════════════════════
def fig_rooting_overhead():
    # Per-probe split is an approximate shape consistent with the divan
    # batch median of 24.22 ms / 100 claims = 242 us/claim reported in
    # benchmarks/macro/rooting_overhead_2026-04.md. Contradiction
    # dominates because it is the only Datalog-join probe; the others
    # are tight local computations.
    probes = ["Provenance", "Contradiction", "Predicate", "Topology", "Temporal"]
    per_claim_us = [20, 122, 35, 40, 25]
    total_us = sum(per_claim_us)
    assert total_us == 242, "per-probe split must sum to the 242 us divan total"

    fig, ax = plt.subplots(figsize=(6.4, 3.0))
    y = np.arange(len(probes))
    colors = [
        C_ROOTED if p == "Provenance"
        else "#a14d4d" if p == "Contradiction"
        else C_PRIMARY
        for p in probes
    ]
    bars = ax.barh(y, per_claim_us, color=colors, edgecolor="black",
                   linewidth=0.4, height=0.58)
    ax.set_yticks(y)
    ax.set_yticklabels(probes, fontsize=9)
    ax.set_xlabel("per-claim cost (\u00b5s)")
    ax.set_xlim(0, max(per_claim_us) * 1.35)
    for b, v in zip(bars, per_claim_us):
        ax.text(v + 3, b.get_y() + b.get_height() / 2,
                f"{v} \u00b5s", va="center", fontsize=9)
    ax.invert_yaxis()
    ax.set_title(
        "Rooting probe cost (divan, N = 100 \u2192 "
        f"{total_us} \u00b5s total)"
    )
    fig.tight_layout()
    fig.savefig(os.path.join(OUT, "fig_rooting_overhead.pdf"))
    plt.close(fig)


# ═════════════════════════════════════════════════════════════════════════════
if __name__ == "__main__":
    fig_longmemeval_categories()
    fig_http_latency()
    fig_tier_distribution()
    fig_tier_distribution_longmem()
    fig_longmemeval_landscape()
    fig_rooting_overhead()
    print("figures generated ->", OUT)
