import { useState } from "react";
import { TabBar, type TabDescriptor } from "./TabBar";
import { SettingsView } from "@/components/settings/SettingsView";
import { PrivacyDashboard } from "@/components/privacy/PrivacyDashboard";
import { useApp } from "@/store/app";
import type { Surface } from "@/types";

const SURFACE_LABELS: Record<Surface, string> = {
  chats: "Chat",
  brain: "Brain",
  satellites: "Satellites",
  trace: "Trace",
  privacy: "Privacy",
  settings: "Settings",
};

/**
 * Main working area — tab bar on top + tab content below.
 *
 * Step 8 ships only the Settings surface end-to-end. Chat lands when
 * the agent-runtime sidecar wires up (Step 10), Brain lands with the
 * privacy dashboard (Step 13), and the remaining surfaces are placeholder
 * "coming soon" screens until their owning steps ship.
 */
export function MainPane() {
  const surface = useApp((s) => s.surface);

  const [tabs, setTabs] = useState<TabDescriptor[]>([
    { id: "t-home", title: "Workspace" },
  ]);
  const [activeId, setActiveId] = useState<string | null>("t-home");

  function closeTab(id: string) {
    setTabs((ts) => {
      const next = ts.filter((t) => t.id !== id);
      if (activeId === id) {
        setActiveId(next[next.length - 1]?.id ?? null);
      }
      return next;
    });
  }

  function newTab() {
    const id = `t-${Date.now()}`;
    setTabs((ts) => [...ts, { id, title: `${SURFACE_LABELS[surface]} ${ts.length + 1}` }]);
    setActiveId(id);
  }

  return (
    <main className="flex h-full min-w-0 flex-1 flex-col bg-background">
      <TabBar
        tabs={tabs}
        activeId={activeId}
        onSelect={setActiveId}
        onClose={closeTab}
        onNew={newTab}
      />
      <div className="flex-1 overflow-hidden">
        {surface === "settings" ? (
          <SettingsView />
        ) : surface === "privacy" ? (
          <PrivacyDashboard />
        ) : (
          <ComingSoon surface={surface} label={SURFACE_LABELS[surface]} />
        )}
      </div>
    </main>
  );
}

function ComingSoon({ surface, label }: { surface: Surface; label: string }) {
  const note: Partial<Record<Surface, string>> = {
    chats: "Lands when the agent-runtime sidecar ships (Step 10).",
    brain: "Lands with the privacy dashboard (Step 13).",
    satellites: "Federation surface — out of scope for v0.1.",
    trace: "Trace + transparency log surface — Step 16.",
  };
  return (
    <div className="flex h-full flex-col items-center justify-center gap-2 px-8 text-center">
      <h2 className="text-base font-medium tracking-tight">{label}</h2>
      <p className="max-w-sm text-sm text-muted-foreground">
        {note[surface] ?? "This surface lands in a later step of the v0.1 plan."}
      </p>
    </div>
  );
}
