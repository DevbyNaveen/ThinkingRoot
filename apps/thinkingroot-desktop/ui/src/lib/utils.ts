import { clsx, type ClassValue } from "clsx";
import { twMerge } from "tailwind-merge";

/**
 * `cn(a, b, c)` — merge conditional Tailwind classes and resolve
 * conflicts correctly (e.g. `p-2` + `p-4` → `p-4`). This is the same
 * utility shadcn/ui components expect to find at `@/lib/utils`.
 */
export function cn(...inputs: ClassValue[]) {
  return twMerge(clsx(inputs));
}

/** Format a USD amount as `$X.XX` or `$X.XXm` (mills for sub-cent). */
export function formatCost(usd: number): string {
  if (usd === 0) return "$0.00";
  if (usd < 0.01) return `$${(usd * 1000).toFixed(2)}m`;
  return `$${usd.toFixed(2)}`;
}

/** `12345 → 12.3k`, `1234567 → 1.2M`. */
export function formatTokens(n: number): string {
  if (n < 1_000) return String(n);
  if (n < 1_000_000) return `${(n / 1_000).toFixed(1)}k`;
  return `${(n / 1_000_000).toFixed(1)}M`;
}
