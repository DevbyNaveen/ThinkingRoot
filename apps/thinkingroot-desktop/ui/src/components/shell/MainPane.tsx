import { ChatView } from "@/components/chat/ChatView";
import { SettingsView } from "@/components/settings/SettingsView";
import { useApp } from "@/store/app";

/**
 * Main working area — always Chat or Settings.
 * Brain / Branches / Privacy have moved to the resizable right panel.
 */
export function MainPane() {
  const surface = useApp((s) => s.surface);

  return (
    <main className="flex h-full min-w-0 flex-1 flex-col bg-background">
      <div className="flex-1 overflow-hidden">
        {surface === "settings" ? <SettingsView /> : <ChatView />}
      </div>
    </main>
  );
}
