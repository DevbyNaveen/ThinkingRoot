import { ChatView } from "@/components/chat/ChatView";
import { BrainView } from "@/components/brain/BrainView";
import { SettingsView } from "@/components/settings/SettingsView";
import { PrivacyDashboard } from "@/components/privacy/PrivacyDashboard";
import { useApp } from "@/store/app";

/**
 * Main working area. Surface-routed pane below a thin window
 * chrome/tab strip from the parent shell. Each surface owns its own
 * header — we don't impose a generic tab bar over them because each
 * pane knows its context (workspace name, conversation title) better
 * than this router.
 */
export function MainPane() {
  const surface = useApp((s) => s.surface);

  return (
    <main className="flex h-full min-w-0 flex-1 flex-col bg-background">
      <div className="flex-1 overflow-hidden">
        {surface === "chats" ? (
          <ChatView />
        ) : surface === "brain" ? (
          <BrainView />
        ) : surface === "privacy" ? (
          <PrivacyDashboard />
        ) : (
          <SettingsView />
        )}
      </div>
    </main>
  );
}
