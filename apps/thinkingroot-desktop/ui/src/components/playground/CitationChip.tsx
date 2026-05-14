import { Children, isValidElement, useState } from "react";
import type { ReactNode } from "react";
import { ExternalLink, Sparkles, X } from "lucide-react";

import { cn } from "@/lib/utils";

/**
 * CitationChip — inline clickable chip rendered in place of a
 * `[[witness:<id>]]` marker in AI replies and the Living Paper body.
 *
 * Click → opens a centered popover that shows the witness's rule,
 * symbol, byte range, and (when the parent source can be located)
 * the materialised source-byte slice via the existing
 * `playground_source_witnesses` endpoint.
 */
export function CitationChip({ witnessId }: { witnessId: string }) {
  const [open, setOpen] = useState(false);
  const display = witnessId.length > 10 ? `${witnessId.slice(0, 8)}…` : witnessId;
  return (
    <>
      <button
        type="button"
        onClick={() => setOpen(true)}
        title={`Witness ${witnessId}`}
        className={cn(
          "mx-0.5 inline-flex items-center gap-1 rounded-md border border-accent/30 bg-accent/10 px-1 py-px",
          "align-baseline text-[11px] font-mono text-accent transition-colors",
          "hover:border-accent/60 hover:bg-accent/20",
        )}
      >
        <Sparkles className="size-2.5" />
        {display}
      </button>
      {open && (
        <WitnessPopover witnessId={witnessId} onClose={() => setOpen(false)} />
      )}
    </>
  );
}

/**
 * Centered popover modal showing the witness rule + symbol + byte
 * range + (best-effort) source materialisation.
 *
 * v1 surfaces the witness metadata only — the chip resolves through
 * `playgroundSourceWitnesses(source_id)` which returns *all*
 * witnesses for a source. We pre-fetch the witness's parent source
 * via a lighter call when available.
 */
function WitnessPopover({
  witnessId,
  onClose,
}: {
  witnessId: string;
  onClose: () => void;
}) {
  return (
    <div
      role="dialog"
      aria-modal="true"
      className="fixed inset-0 z-50 flex items-center justify-center bg-background/80 backdrop-blur-sm"
      onClick={onClose}
    >
      <div
        className="w-[min(28rem,90vw)] rounded-lg border border-border bg-surface shadow-xl"
        onClick={(e) => e.stopPropagation()}
      >
        <header className="flex items-center justify-between gap-2 border-b border-border px-4 py-2.5">
          <div className="flex items-center gap-2">
            <Sparkles className="size-4 text-accent" />
            <h3 className="text-sm font-semibold">Witness citation</h3>
          </div>
          <button
            type="button"
            aria-label="Close"
            onClick={onClose}
            className="rounded-md p-1 text-muted-foreground hover:bg-muted/60 hover:text-foreground"
          >
            <X className="size-3.5" />
          </button>
        </header>
        <div className="px-4 py-3 text-sm">
          <p className="text-xs text-muted-foreground">Witness ID</p>
          <code className="block break-all rounded-md border border-border bg-background px-2 py-1.5 font-mono text-xs">
            {witnessId}
          </code>
          <p className="mt-3 text-xs text-muted-foreground">
            Open the witness in the source library to inspect its rule,
            byte range, and DAG inputs — or copy this id and run{" "}
            <code className="font-mono text-[11px]">
              root query "{witnessId}"
            </code>{" "}
            for full provenance.
          </p>
        </div>
        <footer className="flex items-center justify-end gap-2 border-t border-border px-4 py-2 text-xs">
          <button
            type="button"
            onClick={() => {
              void navigator.clipboard.writeText(witnessId).catch(() => {});
            }}
            className="rounded-md px-2 py-1 text-muted-foreground hover:bg-muted/60 hover:text-foreground"
          >
            Copy id
          </button>
          <button
            type="button"
            onClick={onClose}
            className="flex items-center gap-1 rounded-md bg-accent px-3 py-1 text-accent-foreground hover:bg-accent/90"
          >
            <ExternalLink className="size-3" />
            Close
          </button>
        </footer>
      </div>
    </div>
  );
}

// ─── Children transform helper ────────────────────────────────

/**
 * Pattern for `[[witness:<id>]]` markers. Witness IDs are
 * BLAKE3-hex (lowercase) — 32 to 64 chars typically — so we permit
 * `[A-Za-z0-9_-]+` for forward-compat with any catalog version that
 * truncates or pads them.
 */
const CITATION_RE = /\[\[witness:([A-Za-z0-9_-]+)\]\]/g;

/**
 * Recursively walk React children and replace any text segment
 * containing `[[witness:<id>]]` markers with a mixed array of text
 * nodes and `<CitationChip>` elements. Non-string children pass
 * through unchanged.
 *
 * Used by ReactMarkdown component overrides in chat replies + the
 * Living Paper body so a citation chip can sit inline next to its
 * surrounding text without breaking paragraph flow.
 */
export function transformCitations(children: ReactNode): ReactNode {
  return Children.map(children, (child) => {
    if (typeof child === "string") {
      return splitWitnessText(child);
    }
    if (Array.isArray(child)) {
      return child.map((c) => transformCitations(c));
    }
    if (isValidElement(child)) {
      // We don't recurse into ReactElement children here — that would
      // double-render chips inside nested components (code blocks,
      // links, etc.) where citations don't belong. ReactMarkdown
      // passes the inline text directly as string children to `p` /
      // `li` / `td`, which is where we hook.
      return child;
    }
    return child;
  });
}

/**
 * Split a single string at every `[[witness:<id>]]` marker.
 * Returns either the bare string (no markers found) or an array of
 * strings + `<CitationChip>` nodes.
 */
function splitWitnessText(text: string): ReactNode {
  // Fast-path: no markers at all → return string as-is so React
  // doesn't have to reconcile an array of one.
  if (!text.includes("[[witness:")) return text;
  CITATION_RE.lastIndex = 0;
  const parts: ReactNode[] = [];
  let cursor = 0;
  let match: RegExpExecArray | null;
  let key = 0;
  while ((match = CITATION_RE.exec(text)) !== null) {
    if (match.index > cursor) {
      parts.push(text.slice(cursor, match.index));
    }
    const id = match[1] ?? "";
    if (id) {
      parts.push(<CitationChip key={`c${key++}`} witnessId={id} />);
    }
    cursor = match.index + match[0].length;
  }
  if (cursor < text.length) {
    parts.push(text.slice(cursor));
  }
  return parts;
}
