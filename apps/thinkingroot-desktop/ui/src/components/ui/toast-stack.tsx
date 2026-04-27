import { AnimatePresence, motion } from "framer-motion";
import { X, Info, CheckCircle2, AlertTriangle, CircleAlert } from "lucide-react";
import { useToasts } from "@/store/toast";
import { cn } from "@/lib/utils";

const KIND_CLASS = {
  info: "border-info/40 text-info",
  success: "border-success/40 text-success",
  warn: "border-warn/40 text-warn",
  error: "border-destructive/40 text-destructive",
} as const;

const KIND_ICON = {
  info: Info,
  success: CheckCircle2,
  warn: AlertTriangle,
  error: CircleAlert,
} as const;

/**
 * Bottom-right toast stack. One motion list; toasts spring in and
 * slide out on dismiss. Mounted once at the app root.
 */
export function ToastStack() {
  const items = useToasts((s) => s.items);
  const dismiss = useToasts((s) => s.dismiss);

  return (
    <div
      aria-live="polite"
      aria-label="Notifications"
      className="pointer-events-none fixed bottom-12 right-4 z-50 flex w-[340px] flex-col gap-2"
    >
      <AnimatePresence>
        {items.map((t) => {
          const Icon = KIND_ICON[t.kind];
          return (
            <motion.div
              key={t.id}
              layout
              initial={{ opacity: 0, y: 8, scale: 0.97 }}
              animate={{ opacity: 1, y: 0, scale: 1 }}
              exit={{ opacity: 0, x: 20 }}
              transition={{ type: "spring", stiffness: 400, damping: 30 }}
              className={cn(
                "pointer-events-auto overflow-hidden rounded-lg border bg-surface-elevated shadow-elevated",
                KIND_CLASS[t.kind],
              )}
            >
              <div className="flex items-start gap-2 px-3 py-2.5">
                <Icon className="mt-0.5 size-4 shrink-0" />
                <div className="min-w-0 flex-1">
                  <p className="text-xs font-medium text-foreground">{t.title}</p>
                  {t.body && (
                    <p className="mt-0.5 text-[11px] leading-relaxed text-muted-foreground">
                      {t.body}
                    </p>
                  )}
                </div>
                <button
                  type="button"
                  aria-label="Dismiss"
                  onClick={() => dismiss(t.id)}
                  className="rounded p-0.5 text-muted-foreground hover:bg-muted hover:text-foreground"
                >
                  <X className="size-3" />
                </button>
              </div>
            </motion.div>
          );
        })}
      </AnimatePresence>
    </div>
  );
}
