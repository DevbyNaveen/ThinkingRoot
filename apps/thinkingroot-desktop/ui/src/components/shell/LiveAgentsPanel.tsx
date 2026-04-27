import { useEffect, useRef, useState } from "react";
import { motion, AnimatePresence } from "framer-motion";
import { Users, Bell } from "lucide-react";
import { onAgentActivity, type AgentActivity } from "@/lib/tauri";
import { cn } from "@/lib/utils";

/**
 * Right-rail panel for the **chats** surface — shows specialists the
 * orchestrator currently has in flight + a slot for proactive
 * notifications (capsule grace warnings, recall suggestions, etc.).
 *
 * Subscribes to `agent_activity` directly; the chat surface no longer
 * mirrors this state (Phase D-15 moved the live pill out of the
 * prompt area into the right rail where the user expected it).
 */
export function LiveAgentsPanel() {
  const [activeAgents, setActiveAgents] = useState<string[]>([]);
  const [recentActivity, setRecentActivity] = useState<AgentActivity[]>([]);
  const unlistenRef = useRef<(() => void) | null>(null);

  useEffect(() => {
    let cancelled = false;
    onAgentActivity((payload) => {
      if (payload.kind === "spawn") {
        setActiveAgents((prev) =>
          prev.includes(payload.agent_id) ? prev : [...prev, payload.agent_id],
        );
      } else if (payload.kind === "retire") {
        setActiveAgents((prev) => prev.filter((id) => id !== payload.agent_id));
      }
      // Last 6 events feed the activity log under the badges.
      setRecentActivity((prev) => [payload, ...prev].slice(0, 6));
    }).then((un) => {
      if (cancelled) un();
      else unlistenRef.current = un;
    });
    return () => {
      cancelled = true;
      unlistenRef.current?.();
      unlistenRef.current = null;
    };
  }, []);

  return (
    <section className="flex flex-col border-b border-border">
      <header className="flex items-center gap-2 px-3 pb-2 pt-3">
        <Users className="size-3.5 text-accent" />
        <h3 className="text-[11px] font-semibold uppercase tracking-widest text-muted-foreground">
          Live agents
        </h3>
        <span className="ml-auto text-[10px] text-muted-foreground">
          {activeAgents.length} active
        </span>
      </header>
      <div className="px-3 pb-3">
        {activeAgents.length === 0 ? (
          <p className="text-[11px] text-muted-foreground">
            No specialists running. Ask a research / review / implement
            question to spawn a multi-agent dispatch.
          </p>
        ) : (
          <div className="flex flex-wrap gap-1">
            <AnimatePresence>
              {activeAgents.map((id) => (
                <motion.span
                  key={id}
                  initial={{ opacity: 0, scale: 0.85 }}
                  animate={{ opacity: 1, scale: 1 }}
                  exit={{ opacity: 0, scale: 0.85 }}
                  className="rounded-full bg-accent/15 px-2 py-0.5 font-mono text-[10px] tracking-tight text-accent"
                >
                  {id}
                </motion.span>
              ))}
            </AnimatePresence>
          </div>
        )}
      </div>

      {recentActivity.length > 0 && (
        <div className="border-t border-border px-3 pb-3 pt-2">
          <h4 className="pb-1 text-[10px] font-semibold uppercase tracking-widest text-muted-foreground">
            Recent
          </h4>
          <ul className="flex flex-col gap-0.5">
            {recentActivity.map((ev, i) => (
              <li
                key={i}
                className="truncate font-mono text-[10px] text-muted-foreground"
              >
                {describeActivity(ev)}
              </li>
            ))}
          </ul>
        </div>
      )}

      <NotificationsSlot />
    </section>
  );
}

function describeActivity(ev: AgentActivity): string {
  switch (ev.kind) {
    case "spawn":
      return `+ ${ev.agent_id}`;
    case "retire":
      return `– ${ev.agent_id}`;
    case "speak":
      return `${ev.from} → ${ev.to}`;
    case "read":
      return `${ev.agent_id} read ${ev.claim_id}`;
    case "write":
      return `${ev.agent_id} wrote ${ev.claim_id}`;
  }
}

/**
 * Proactive notifications slot — capsule grace warnings, recall
 * suggestions, peer-presence changes, etc. Empty until the
 * notifications backend lands; rendered as a placeholder so the
 * layout settles into its final position now and doesn't shift later.
 */
function NotificationsSlot() {
  return (
    <div className={cn("border-t border-border px-3 pb-3 pt-2")}>
      <h4 className="flex items-center gap-1.5 pb-1 text-[10px] font-semibold uppercase tracking-widest text-muted-foreground">
        <Bell className="size-3" />
        Notifications
      </h4>
      <p className="text-[11px] text-muted-foreground">
        Quiet. New capsules, blindspot scans, and peer events surface here
        as they happen.
      </p>
    </div>
  );
}
