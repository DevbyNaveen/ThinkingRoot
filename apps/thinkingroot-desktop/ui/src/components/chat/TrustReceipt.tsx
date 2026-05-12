// apps/thinkingroot-desktop/ui/src/components/chat/TrustReceipt.tsx
//
// Trust-receipt chip + modal — the visible tip of the verifier
// pipeline that lives at intelligence/verifier.rs in the engine.
//
// Wire path (engine → UI):
//
//   engine: agent_stream_response emits `event: trust_receipt` after
//     `event: final` (rest.rs::agent_stream_response wiring).
//   desktop sidecar: consume_ask_stream parses the SSE event into
//     ChatEvent::TrustReceipt (commands/chat.rs).
//   desktop UI: ChatView's chat-event listener attaches the receipt
//     to the matching assistant message; this component renders.
//
// Render contract:
//
//   * fully_grounded     → 🔒 green   "Grounded · N claims"
//   * partially_grounded → 🔒 yellow  "Partial · N claims · M related"
//   * unverified_citations → 🔒 red   "Unverified · K bad citations"
//   * skipped_*          → no chip    (cleanly invisible — nothing to attest)
//
// Click the chip → modal listing claim_ids + (where applicable) the
// bad_claim_ids that didn't resolve.

import { useState } from "react";

import { cn } from "@/lib/utils";
import type { TrustReceipt as TrustReceiptShape, TrustReceiptKind } from "../../types";

type ChipPalette = {
  bg: string;
  border: string;
  fg: string;
  glyph: string;
  label: string;
  description: string;
};

function paletteFor(kind: TrustReceiptKind, receipt: TrustReceiptShape): ChipPalette | null {
  switch (kind) {
    case "fully_grounded": {
      const auto = receipt.autoCitedCount ?? 0;
      const total = receipt.claimsUsed.length;
      return {
        bg: "bg-emerald-50 dark:bg-emerald-950/40",
        border: "border-emerald-300 dark:border-emerald-800",
        fg: "text-emerald-700 dark:text-emerald-300",
        glyph: "🔒",
        label: `Grounded · ${total} claim${total === 1 ? "" : "s"}`,
        description:
          auto > 0
            ? `Every cited claim resolves in substrate. ${auto} auto-cited from retrieval, ${
                total - auto
              } cited explicitly.`
            : "Every cited claim resolves in substrate.",
      };
    }
    case "partially_grounded": {
      const total = receipt.claimsUsed.length;
      const related = receipt.relatedCount ?? 0;
      return {
        bg: "bg-amber-50 dark:bg-amber-950/40",
        border: "border-amber-300 dark:border-amber-800",
        fg: "text-amber-700 dark:text-amber-300",
        glyph: "🔒",
        label: `Partial · ${total} claim${total === 1 ? "" : "s"} · ${related} related`,
        description:
          "At least one citation rests on related context (vocabulary match, not strict entailment). The model's reading may be approximate.",
      };
    }
    case "unverified_citations": {
      const bad = receipt.badClaimIds?.length ?? 0;
      return {
        bg: "bg-rose-50 dark:bg-rose-950/40",
        border: "border-rose-300 dark:border-rose-800",
        fg: "text-rose-700 dark:text-rose-300",
        glyph: "🔒",
        label: `Unverified · ${bad} bad citation${bad === 1 ? "" : "s"}`,
        description:
          "The model emitted citations that don't resolve in substrate, or the response wasn't grounded in any retrievable claim.",
      };
    }
    case "skipped_chitchat":
    case "skipped_rejection":
    case "skipped_bench":
      return null; // invisible — no claim to attest
  }
}

export function TrustReceiptChip({ receipt }: { receipt: TrustReceiptShape }) {
  const [open, setOpen] = useState(false);
  const palette = paletteFor(receipt.kind, receipt);
  if (!palette) return null;

  return (
    <>
      <button
        type="button"
        onClick={() => setOpen(true)}
        title={palette.description}
        className={cn(
          "inline-flex cursor-pointer items-center gap-1.5 border-0 bg-transparent p-0 text-left text-xs font-normal text-muted-foreground",
          "transition-colors hover:text-foreground hover:underline hover:underline-offset-2",
        )}
        aria-label={`Trust receipt: ${palette.label}. Click for details.`}
      >
        <span>{palette.label}</span>
      </button>
      {open && (
        <TrustReceiptModal
          receipt={receipt}
          palette={palette}
          onClose={() => setOpen(false)}
        />
      )}
    </>
  );
}

function TrustReceiptModal({
  receipt,
  palette,
  onClose,
}: {
  receipt: TrustReceiptShape;
  palette: ChipPalette;
  onClose: () => void;
}) {
  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/40 backdrop-blur-sm"
      onClick={onClose}
      role="dialog"
      aria-modal="true"
      aria-labelledby="trust-receipt-title"
    >
      <div
        className="max-h-[80vh] w-[min(560px,90vw)] overflow-y-auto rounded-lg border border-zinc-200 bg-white p-5 shadow-2xl dark:border-zinc-800 dark:bg-zinc-900"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="mb-3 flex items-start justify-between">
          <div>
            <div className="flex items-center gap-2">
              <span aria-hidden className="text-lg">
                {palette.glyph}
              </span>
              <h2 id="trust-receipt-title" className="text-base font-semibold">
                {palette.label}
              </h2>
            </div>
            <p className="mt-1 text-sm text-zinc-600 dark:text-zinc-400">
              {palette.description}
            </p>
          </div>
          <button
            type="button"
            onClick={onClose}
            aria-label="Close"
            className="ml-3 rounded p-1 text-zinc-500 hover:bg-zinc-100 hover:text-zinc-900 dark:hover:bg-zinc-800 dark:hover:text-zinc-100"
          >
            ✕
          </button>
        </div>

        {receipt.claimsUsed.length > 0 && (
          <section className="mt-4">
            <h3 className="mb-2 text-xs font-semibold uppercase tracking-wide text-zinc-500 dark:text-zinc-500">
              Claims used ({receipt.claimsUsed.length})
            </h3>
            <ul className="space-y-1 font-mono text-xs">
              {receipt.claimsUsed.map((id) => (
                <li
                  key={id}
                  className="rounded border border-zinc-200 bg-zinc-50 px-2 py-1 text-zinc-800 dark:border-zinc-800 dark:bg-zinc-950 dark:text-zinc-200"
                >
                  {id}
                </li>
              ))}
            </ul>
          </section>
        )}

        {receipt.kind === "unverified_citations" &&
          receipt.badClaimIds &&
          receipt.badClaimIds.length > 0 && (
            <section className="mt-4">
              <h3 className="mb-2 text-xs font-semibold uppercase tracking-wide text-rose-700 dark:text-rose-400">
                Hallucinated citations ({receipt.badClaimIds.length})
              </h3>
              <ul className="space-y-1 font-mono text-xs">
                {receipt.badClaimIds.map((id) => (
                  <li
                    key={id}
                    className="rounded border border-rose-300 bg-rose-50 px-2 py-1 text-rose-800 dark:border-rose-800 dark:bg-rose-950/40 dark:text-rose-200"
                  >
                    {id}
                  </li>
                ))}
              </ul>
              <p className="mt-2 text-xs text-zinc-600 dark:text-zinc-400">
                These claim ids appeared in the response but don't exist in
                substrate. Treat the surrounding sentence as unverified.
              </p>
            </section>
          )}

        {receipt.kind === "fully_grounded" && (receipt.autoCitedCount ?? 0) > 0 && (
          <p className="mt-4 text-xs text-zinc-600 dark:text-zinc-400">
            <strong>{receipt.autoCitedCount}</strong> of{" "}
            <strong>{receipt.claimsUsed.length}</strong> were auto-cited from
            retrieval (model didn't quote them explicitly but the verifier
            confirmed the grounding).
          </p>
        )}

        {receipt.kind === "partially_grounded" && (receipt.relatedCount ?? 0) > 0 && (
          <p className="mt-4 text-xs text-zinc-600 dark:text-zinc-400">
            <strong>{receipt.relatedCount}</strong> citation
            {receipt.relatedCount === 1 ? " was" : "s were"} below the
            confidence floor — surfaced as related context rather than direct
            evidence.
          </p>
        )}
      </div>
    </div>
  );
}
