import type { ImgHTMLAttributes } from "react";
import { cn } from "@/lib/utils";

/**
 * ThinkingRoot mark for rail tabs and palette rows — uses the same
 * `/logo.png` asset as the window chrome so Knowledge reads as “root”.
 */
export function ThinkingRootGlyph({
  className,
  ...props
}: ImgHTMLAttributes<HTMLImageElement>) {
  return (
    <img
      src="/logo.png"
      alt=""
      draggable={false}
      {...props}
      className={cn("pointer-events-none object-contain", className)}
    />
  );
}
