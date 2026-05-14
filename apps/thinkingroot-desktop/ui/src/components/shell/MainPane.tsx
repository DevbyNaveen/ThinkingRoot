import { ChatView } from "@/components/chat/ChatView";
import { DocsView } from "@/components/docs/DocsView";
import { PlaygroundView } from "@/components/playground/PlaygroundView";
import { SettingsView } from "@/components/settings/SettingsView";
import { useApp } from "@/store/app";

/**
 * Main working area — Chat / Playground / Settings / Docs.
 * Brain / Branches / Privacy live in the resizable right panel.
 */
export function MainPane() {
  const surface = useApp((s) => s.surface);

  return (
    <main className="flex h-full min-w-0 flex-1 flex-col bg-background">
      <div className="flex-1 overflow-hidden">
        {surface === "settings" ? (
          <SettingsView />
        ) : surface === "docs" ? (
          <DocsView />
        ) : surface === "playground" ? (
          <PlaygroundView />
        ) : (
          <ChatView />
        )}
      </div>
    </main>
  );
}
