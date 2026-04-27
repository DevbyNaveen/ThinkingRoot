import { useEffect, useState } from "react";
import { TooltipProvider } from "@/components/ui/tooltip";
import { Sidebar } from "@/components/shell/Sidebar";
import { MainPane } from "@/components/shell/MainPane";
import { RightRail } from "@/components/shell/RightRail";
import { StatusBar } from "@/components/shell/StatusBar";
import { CommandPalette } from "@/components/command-palette/CommandPalette";
import { ToastStack } from "@/components/ui/toast-stack";
import { OnboardingWizard } from "@/components/onboarding/OnboardingWizard";
import { InstallTrSheet } from "@/components/install/InstallTrSheet";
import { onTrFileOpened, onboardingStatus } from "@/lib/tauri";
import { useApp } from "@/store/app";

/**
 * Desktop app root. Three horizontal regions inside a vertical
 * column:
 *
 *   +-------------------------------------------------+
 *   |      draggable title bar (macOS overlay)        |
 *   | rail | sidebar |     main pane     | right rail |
 *   |      |         |                   |            |
 *   +------+---------+-------------------+------------+
 *   |                     status bar                  |
 *   +-------------------------------------------------+
 *
 * Rail + status bar are always visible. Sidebar and right rail are
 * independently collapsible. The main pane hosts a tab bar and a
 * content area (chat / brain / trace / …) that reacts to the active
 * surface + active tab.
 */
export default function App() {
  const theme = useApp((s) => s.theme);
  const onboardingOpen = useApp((s) => s.onboardingOpen);
  const setOnboardingOpen = useApp((s) => s.setOnboardingOpen);
  const onboardingDismissed = useApp((s) => s.onboardingDismissed);
  const setOnboardingDismissed = useApp((s) => s.setOnboardingDismissed);
  const [installTrPath, setInstallTrPath] = useState<string | null>(null);

  // Subscribe to `tr-file-opened` events emitted by the Rust side
  // when a `.tr` file is dropped on the window or routed via the
  // OS file association.
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    onTrFileOpened((path) => setInstallTrPath(path)).then((un) => {
      unlisten = un;
    });
    return () => {
      unlisten?.();
    };
  }, []);

  // First-launch detection — open the wizard if no provider key is
  // configured and the user hasn't already skipped it.
  useEffect(() => {
    if (onboardingDismissed) return;
    let cancelled = false;
    (async () => {
      try {
        const status = await onboardingStatus();
        if (cancelled) return;
        if (!status.has_any_provider_key) {
          setOnboardingOpen(true);
        }
      } catch {
        // Tauri may not be available in test renderers — ignore.
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [onboardingDismissed, setOnboardingOpen]);

  // Re-apply theme on mount so the <html data-theme> attribute is
  // hydrated from persisted store even on first paint.
  useEffect(() => {
    const resolved =
      theme === "auto"
        ? window.matchMedia("(prefers-color-scheme: light)").matches
          ? "light"
          : "dark"
        : theme;
    document.documentElement.dataset.theme = resolved;
  }, [theme]);

  return (
    <TooltipProvider delayDuration={250} skipDelayDuration={120}>
      <div className="flex h-full w-full flex-col bg-background text-foreground">
        <div className="flex min-h-0 flex-1">
          <Sidebar />
          <MainPane />
          <RightRail />
        </div>
        <StatusBar />
      </div>
      <CommandPalette />
      <OnboardingWizard
        open={onboardingOpen}
        onComplete={() => {
          setOnboardingOpen(false);
          setOnboardingDismissed(true);
        }}
        onSkip={() => {
          setOnboardingOpen(false);
          setOnboardingDismissed(true);
        }}
      />
      <InstallTrSheet
        path={installTrPath}
        onClose={() => setInstallTrPath(null)}
      />
      <ToastStack />
    </TooltipProvider>
  );
}
