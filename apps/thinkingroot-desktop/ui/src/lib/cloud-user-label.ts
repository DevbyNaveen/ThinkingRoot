import type { AuthState } from "@/lib/tauri";

/** First name for sidebar chrome — hub display_name, else title-cased handle. */
export function cloudUserFirstName(auth: AuthState): string {
  const fromDisplay = auth.display_name?.trim().split(/\s+/)[0];
  if (fromDisplay) return fromDisplay;

  const handle = auth.handle?.trim();
  if (!handle) return "Signed in";

  const token = handle.split(/[_.-]/)[0] ?? handle;
  if (!token) return "Signed in";
  return token.charAt(0).toUpperCase() + token.slice(1).toLowerCase();
}
