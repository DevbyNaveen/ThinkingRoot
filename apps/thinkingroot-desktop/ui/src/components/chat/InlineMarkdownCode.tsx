import type { ReactNode } from "react";

import { cn } from "@/lib/utils";
import { isFileLikeInlineCode } from "./inline-reference";

export function InlineMarkdownCode({
  text,
  className,
  ...props
}: {
  text: string;
  className?: string;
  children?: ReactNode;
}) {
  const fileLike = isFileLikeInlineCode(text);

  if (fileLike) {
    return (
      <code
        className={cn(
          "inline-reference font-mono text-[0.9em] font-normal text-foreground/88",
          className,
        )}
        title={text}
        {...props}
      >
        {text}
      </code>
    );
  }

  return (
    <code
      className={cn(
        "inline-code rounded-sm bg-foreground/[0.06] px-1 py-px font-mono text-[0.86em] text-foreground/92",
        className,
      )}
      {...props}
    >
      {text}
    </code>
  );
}
