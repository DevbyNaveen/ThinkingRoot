import { AlertTriangle, Loader2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import type { PrivacySource } from "@/lib/tauri";

interface Props {
  source: PrivacySource | null;
  forgetting: boolean;
  onConfirm: () => void;
  onCancel: () => void;
}

/**
 * Confirmation dialog before forgetting a source. Emphasises the
 * action's irreversibility — there is no soft-delete tombstone in
 * the engine; the row and every descendant claim, entity edge, and
 * vector are removed in one transaction.
 */
export function ForgetDialog({ source, forgetting, onConfirm, onCancel }: Props) {
  if (!source) return null;
  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-label="Confirm forget"
      className="fixed inset-0 z-[58] flex items-center justify-center bg-background/70 backdrop-blur-sm"
      onClick={(e) => {
        if (e.target === e.currentTarget && !forgetting) onCancel();
      }}
    >
      <div className="w-full max-w-md overflow-hidden rounded-xl border border-border bg-surface-elevated shadow-elevated">
        <div className="px-5 py-4">
          <header className="flex items-start gap-3">
            <div className="flex size-9 shrink-0 items-center justify-center rounded-md bg-destructive/15 text-destructive">
              <AlertTriangle className="size-4" />
            </div>
            <div className="min-w-0">
              <h3 className="text-sm font-medium tracking-tight">
                Forget this source?
              </h3>
              <p className="mt-1 text-[11px] leading-relaxed text-muted-foreground">
                Removes the source row plus every claim, entity edge, vector,
                and contradiction that descends from it. This cannot be undone
                — there is no tombstone.
              </p>
            </div>
          </header>
          <div className="mt-4 rounded-md border border-border bg-background p-3">
            <p className="font-mono text-[11px] text-foreground" title={source.uri}>
              {source.uri}
            </p>
            <p className="mt-1 text-[10px] text-muted-foreground">
              {source.source_type} · {source.id}
            </p>
          </div>
        </div>
        <footer className="flex items-center justify-end gap-2 border-t border-border bg-surface px-5 py-3">
          <Button
            variant="outline"
            size="sm"
            onClick={onCancel}
            disabled={forgetting}
            className="h-8 text-xs"
          >
            Cancel
          </Button>
          <Button
            size="sm"
            onClick={onConfirm}
            disabled={forgetting}
            className="h-8 gap-1 bg-destructive text-xs text-destructive-foreground hover:bg-destructive/90"
          >
            {forgetting ? (
              <>
                <Loader2 className="size-3 animate-spin" /> Forgetting…
              </>
            ) : (
              "Forget"
            )}
          </Button>
        </footer>
      </div>
    </div>
  );
}
