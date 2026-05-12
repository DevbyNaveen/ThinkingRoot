import { forwardRef } from "react";
import type { LucideProps } from "lucide-react";
import { RotateCw } from "lucide-react";
import { cn } from "@/lib/utils";

/**
 * Unified reload / refresh glyph for the desktop shell. Uses a single-arc
 * rotate arrow (lighter stroke) instead of the legacy twin-arrow `RefreshCw`
 * so toolbars stay visually consistent and a bit more modern.
 */
export const RefreshIcon = forwardRef<SVGSVGElement, LucideProps>(
  ({ className, strokeWidth = 1.65, ...props }, ref) => (
    <RotateCw
      ref={ref}
      strokeWidth={strokeWidth}
      className={cn(className)}
      {...props}
    />
  ),
);

RefreshIcon.displayName = "RefreshIcon";
