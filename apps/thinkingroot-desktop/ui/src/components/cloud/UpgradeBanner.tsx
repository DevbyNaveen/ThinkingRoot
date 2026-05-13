import { Sparkles } from "lucide-react";

import { Button } from "@/components/ui/button";
import { cloudOpenUpgrade } from "@/lib/tauri";

/**
 * Three failure shapes the hub surfaces that the desktop renders as
 * an inline upgrade prompt rather than a raw error string:
 *
 *   - `credits_exhausted` — HTTP 402 from the routed model call. Hub
 *     returns the credit budget needed vs remaining so the user sees
 *     exactly what tipped them over.
 *   - `tier_required`     — HTTP 403 from a feature gated to Pro
 *     (e.g. private packs, large-context models).
 *   - `private_pack_requires_pro` — pre-flight check the hub runs
 *     before allowing a push with `--visibility private`.
 *
 * Spec: docs/superpowers/specs/2026-05-13-oss-cloud-readiness-design.md §7.5 + §8.6.
 */
export type UpgradeReason =
  | { kind: "credits_exhausted"; needed: number; remaining: number }
  | { kind: "tier_required"; feature: string }
  | { kind: "private_pack_requires_pro" };

export interface UpgradeBannerProps {
  reason: UpgradeReason;
  onSwitchProvider?: () => void;
}

export function UpgradeBanner({ reason, onSwitchProvider }: UpgradeBannerProps) {
  const headline = (() => {
    switch (reason.kind) {
      case "credits_exhausted":
        return `Out of credits — needed ${reason.needed.toLocaleString()}, only ${reason.remaining.toLocaleString()} left this cycle.`;
      case "tier_required":
        return `Pro tier required for ${reason.feature}.`;
      case "private_pack_requires_pro":
        return "Private packs are a Pro-tier feature.";
    }
  })();

  return (
    <div className="rounded-md border border-amber-300 bg-amber-50 p-3 text-sm space-y-2">
      <div className="flex items-center gap-2 font-medium text-amber-900">
        <Sparkles className="h-4 w-4" />
        {headline}
      </div>
      <p className="text-xs text-amber-800">
        Upgrade to Pro for 50,000 credits / month and access to the full model
        catalogue. You can also switch to a BYOK provider with your own API key.
      </p>
      <div className="flex gap-2">
        <Button size="sm" onClick={() => cloudOpenUpgrade()}>
          Upgrade to Pro
        </Button>
        {onSwitchProvider && (
          <Button size="sm" variant="outline" onClick={onSwitchProvider}>
            Switch to BYOK provider
          </Button>
        )}
      </div>
    </div>
  );
}

/**
 * Parse a stringified backend error into a typed UpgradeReason.
 * Returns null when the error is not upgrade-related — callers
 * should fall back to their generic error renderer.
 *
 * The matching is intentionally lenient: the hub's wire payload is
 * the source of truth, but engine-side surfaces may stringify the
 * error in transit (Tauri command Result -> String, SSE
 * `event: error` text frames). The regex parser handles both.
 */
export function parseUpgradeReason(error: unknown): UpgradeReason | null {
  if (typeof error !== "string") return null;
  const lower = error.toLowerCase();
  if (
    lower.includes("credits exhausted") ||
    lower.includes("credits_exhausted")
  ) {
    const match = error.match(/needed (\d+).*remaining (\d+)/i);
    if (match && match[1] && match[2]) {
      return {
        kind: "credits_exhausted",
        needed: parseInt(match[1], 10),
        remaining: parseInt(match[2], 10),
      };
    }
    return { kind: "credits_exhausted", needed: 0, remaining: 0 };
  }
  if (
    lower.includes("private pack") &&
    (lower.includes("pro") || lower.includes("private_pack_requires_pro"))
  ) {
    return { kind: "private_pack_requires_pro" };
  }
  if (
    lower.includes("pro tier required") ||
    lower.includes("tier_required")
  ) {
    const match = error.match(/feature\s+`?([^`'"\s]+)`?/i);
    return {
      kind: "tier_required",
      feature: match?.[1] ?? "this feature",
    };
  }
  return null;
}
