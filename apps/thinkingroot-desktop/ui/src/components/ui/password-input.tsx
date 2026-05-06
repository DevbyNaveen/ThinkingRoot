/**
 * Password input with a built-in show/hide toggle.
 *
 * Drop-in replacement for `<input type="password" />` — accepts every
 * native input prop except `type`. Internal state tracks the
 * visible/hidden flag; default is hidden. Toggle button is a
 * presentational eye icon, focusable but `tabIndex={-1}` so it
 * doesn't steal tab focus from the surrounding form.
 *
 * The button never resets to "hidden" automatically — a user who
 * unmasks once and tabs away keeps their preference for the lifetime
 * of the component instance. We intentionally don't persist it
 * across mounts because credentials are sensitive enough that
 * re-rendering the same form on a different visit should default to
 * masked again.
 */
import { forwardRef, useState } from "react";
import { Eye, EyeOff } from "lucide-react";

import { cn } from "@/lib/utils";

type Props = Omit<React.InputHTMLAttributes<HTMLInputElement>, "type">;

export const PasswordInput = forwardRef<HTMLInputElement, Props>(function PasswordInput(
  { className, ...rest },
  ref,
) {
  const [visible, setVisible] = useState(false);

  return (
    <div className="relative w-full">
      <input
        ref={ref}
        type={visible ? "text" : "password"}
        autoComplete="off"
        spellCheck={false}
        className={cn("pr-9", className)}
        {...rest}
      />
      <button
        type="button"
        onClick={() => setVisible((v) => !v)}
        tabIndex={-1}
        aria-label={visible ? "Hide value" : "Show value"}
        title={visible ? "Hide" : "Show"}
        className="absolute right-1.5 top-1/2 flex size-7 -translate-y-1/2 items-center justify-center rounded text-muted-foreground transition-colors hover:bg-muted hover:text-foreground"
      >
        {visible ? <EyeOff className="size-3.5" /> : <Eye className="size-3.5" />}
      </button>
    </div>
  );
});
