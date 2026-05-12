import type { SVGProps } from "react";
import { cn } from "@/lib/utils";

/**
 * App-specific Knowledge affordance — compact “substrate bump” (one
 * root, two leaves) so the rail avoids yet another generic Lucide icon.
 * Uses `currentColor` like Lucide for theme + active states.
 */
export function KnowledgeMark({ className, ...rest }: SVGProps<SVGSVGElement>) {
  return (
    <svg
      viewBox="0 0 24 24"
      fill="none"
      aria-hidden
      className={cn("shrink-0", className)}
      {...rest}
    >
      <circle cx="12" cy="6.75" r="2.35" fill="currentColor" opacity={0.92} />
      <circle cx="6.75" cy="17" r="2.05" fill="currentColor" opacity={0.78} />
      <circle cx="17.25" cy="17" r="2.05" fill="currentColor" opacity={0.78} />
      <path
        d="M12 8.85 6.95 15.15M12 8.85l5.05 6.3"
        stroke="currentColor"
        strokeWidth={1.65}
        strokeLinecap="round"
        opacity={0.88}
      />
    </svg>
  );
}
