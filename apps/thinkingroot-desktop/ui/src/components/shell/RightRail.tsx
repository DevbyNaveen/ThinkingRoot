import { PanelRight, Link2, Sparkles, FileText, ExternalLink } from "lucide-react";
import { motion, AnimatePresence } from "framer-motion";
import { useApp } from "@/store/app";
import { cn } from "@/lib/utils";
import { Button } from "@/components/ui/button";
import type { ChatMessage, Provenance } from "@/types";
import { LiveAgentsPanel } from "./LiveAgentsPanel";

const TIER_BADGE: Record<Provenance["tier"], string> = {
  rooted: "bg-tier-rooted/15 text-tier-rooted border-tier-rooted/30",
  attested: "bg-tier-attested/15 text-tier-attested border-tier-attested/30",
  unknown: "bg-tier-unknown/15 text-tier-unknown border-tier-unknown/30",
};

/**
 * Right inspector rail. Shows the provenance pills belonging to the
 * most recent assistant message in the active conversation, plus a
 * detail drawer for the selected pill.
 */
export function RightRail() {
  const open = useApp((s) => s.rightRailOpen);
  const toggle = useApp((s) => s.toggleRightRail);
  const surface = useApp((s) => s.surface);
  const activeConvId = useApp((s) => s.activeConversationId);
  const messagesByConv = useApp((s) => s.messages);
  const selectedClaimId = useApp((s) => s.selectedClaimId);
  const setSelectedClaimId = useApp((s) => s.setSelectedClaimId);

  if (!open) {
    return (
      <div className="flex h-full w-10 shrink-0 flex-col items-center border-l border-border bg-surface">
        <header className="flex h-11 w-full items-center justify-center border-b border-border">
          <Button
            variant="ghost"
            size="icon"
            onClick={toggle}
            aria-label="Show inspector"
            className="h-7 w-7"
          >
            <PanelRight className="size-3.5" />
          </Button>
        </header>
      </div>
    );
  }

  const messages: ChatMessage[] = activeConvId
    ? (messagesByConv[activeConvId] ?? [])
    : [];
  const latestAssistant = [...messages]
    .reverse()
    .find((m) => m.kind === "assistant" && (m.provenance?.length ?? 0) > 0);
  const claims = latestAssistant?.provenance ?? [];
  const selected = claims.find((c) => c.claimId === selectedClaimId);

  return (
    <aside
      className="flex h-full w-80 shrink-0 flex-col border-l border-border bg-surface"
      aria-label="Inspector"
    >
      <header className="flex h-11 items-center justify-between gap-2 border-b border-border px-3">
        <div className="flex items-center gap-2">
          <Link2 className="size-4 text-muted-foreground" />
          <h2 className="text-sm font-medium tracking-tight">
            Provenance
          </h2>
          {claims.length > 0 && (
            <span className="text-[10px] text-muted-foreground">
              {claims.length} claim{claims.length === 1 ? "" : "s"}
            </span>
          )}
        </div>
        <Button
          variant="ghost"
          size="icon"
          onClick={toggle}
          aria-label="Hide inspector"
          className="h-7 w-7"
        >
          <PanelRight className="size-3.5" />
        </Button>
      </header>

      <div className="flex flex-1 flex-col overflow-y-auto">
        {surface === "chats" && <LiveAgentsPanel />}
        {claims.length === 0 ? (
          <EmptyProvenance />
        ) : (
          <>
            <ClaimList
              claims={claims}
              selectedId={selectedClaimId}
              onSelect={setSelectedClaimId}
            />
            <AnimatePresence>
              {selected && (
                <ClaimDetail
                  claim={selected}
                  onClose={() => setSelectedClaimId(null)}
                />
              )}
            </AnimatePresence>
          </>
        )}
      </div>
    </aside>
  );
}

function ClaimList({
  claims,
  selectedId,
  onSelect,
}: {
  claims: Provenance[];
  selectedId: string | null;
  onSelect: (id: string | null) => void;
}) {
  return (
    <div className="flex-1 overflow-y-auto px-3 py-2">
      <ul className="flex flex-col gap-1">
        {claims.map((c) => {
          const active = c.claimId === selectedId;
          return (
            <li key={c.claimId}>
              <button
                type="button"
                onClick={() => onSelect(active ? null : c.claimId)}
                className={cn(
                  "w-full rounded-md border border-transparent px-2 py-1.5 text-left text-xs transition-colors",
                  "hover:bg-muted/60",
                  active && "border-accent/40 bg-accent/5",
                )}
              >
                <div className="flex items-center gap-1.5">
                  <span
                    className={cn(
                      "rounded-full border px-1.5 py-px text-[10px] capitalize",
                      TIER_BADGE[c.tier],
                    )}
                  >
                    {c.tier}
                  </span>
                  <span className="font-mono text-[11px] text-foreground">
                    {c.claimId}
                  </span>
                  <span className="ml-auto text-[10px] text-muted-foreground">
                    {c.confidence.toFixed(2)}
                  </span>
                </div>
                {c.statement && (
                  <p className="mt-1 line-clamp-2 text-[11px] leading-relaxed text-muted-foreground">
                    {c.statement}
                  </p>
                )}
              </button>
            </li>
          );
        })}
      </ul>
    </div>
  );
}

function ClaimDetail({
  claim,
  onClose,
}: {
  claim: Provenance;
  onClose: () => void;
}) {
  return (
    <motion.section
      initial={{ height: 0, opacity: 0 }}
      animate={{ height: "auto", opacity: 1 }}
      exit={{ height: 0, opacity: 0 }}
      transition={{ type: "spring", stiffness: 400, damping: 35 }}
      className="overflow-hidden border-t border-border bg-surface-elevated"
      aria-label="Claim detail"
    >
      <div className="px-4 py-3">
        <header className="flex items-center gap-2">
          <FileText className="size-4 text-accent" />
          <h3 className="text-xs font-medium tracking-tight">Claim source</h3>
          <button
            type="button"
            onClick={onClose}
            aria-label="Close claim detail"
            className="ml-auto rounded px-1.5 py-0.5 text-[10px] text-muted-foreground hover:bg-muted hover:text-foreground"
          >
            close
          </button>
        </header>
        <div className="mt-2 flex items-center gap-1.5">
          <span
            className={cn(
              "rounded-full border px-1.5 py-px text-[10px] capitalize",
              TIER_BADGE[claim.tier],
            )}
          >
            {claim.tier}
          </span>
          <span className="font-mono text-[11px] text-foreground">
            {claim.claimId}
          </span>
          <span className="ml-auto text-[10px] text-muted-foreground">
            confidence {claim.confidence.toFixed(2)}
          </span>
        </div>
        {claim.statement && (
          <blockquote className="mt-3 border-l-2 border-border pl-3 text-[12px] leading-relaxed text-foreground">
            {claim.statement}
          </blockquote>
        )}
        {claim.source && (
          <a
            href={sourceHref(claim.source)}
            target="_blank"
            rel="noreferrer"
            className="mt-3 inline-flex items-center gap-1.5 text-[11px] text-accent hover:underline"
          >
            <ExternalLink className="size-3" />
            <span className="max-w-[220px] truncate font-mono">
              {claim.source}
            </span>
          </a>
        )}
      </div>
    </motion.section>
  );
}

function sourceHref(source: string): string {
  // `source` is a URI-ish string from thinkingroot (`file://…` or
  // `mcp://agent/sid`). Pass through as-is; invalid ones just 404.
  if (source.startsWith("http") || source.startsWith("file:") || source.startsWith("mcp:")) {
    return source;
  }
  return `file://${source}`;
}

function EmptyProvenance() {
  return (
    <div className="flex flex-1 flex-col gap-6 overflow-y-auto px-4 py-6">
      <div className="flex flex-col items-start gap-2 rounded-lg border border-dashed border-border/70 p-4">
        <div className="flex size-8 items-center justify-center rounded-md bg-accent/10 text-accent">
          <Sparkles className="size-4" />
        </div>
        <h3 className="text-sm font-medium">No claims recalled yet</h3>
        <p className="text-xs leading-relaxed text-muted-foreground">
          Set{" "}
          <code className="rounded bg-muted px-1 font-mono text-[10px]">
            THINKINGROOT_WORKSPACE
          </code>{" "}
          to a thinkingroot workspace path. Provenance pills for each recalled
          claim will appear here once your next message triggers a recall.
        </p>
      </div>
    </div>
  );
}
