// shadcn/ui Input.
import { forwardRef } from "react";
import type { InputHTMLAttributes } from "react";

import { cn } from "../../lib/utils";

/** A styled text input that preserves native semantics + a11y. */
export const Input = forwardRef<
  HTMLInputElement,
  InputHTMLAttributes<HTMLInputElement>
>(function Input({ className, type, ...props }, ref) {
  return (
    <input
      ref={ref}
      type={type}
      className={cn(
        "flex h-9 w-full rounded-md border border-input bg-transparent px-3 py-1 text-sm shadow-sm transition-colors placeholder:text-muted-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring disabled:cursor-not-allowed disabled:opacity-50 aria-[invalid=true]:border-destructive aria-[invalid=true]:ring-destructive",
        className,
      )}
      {...props}
    />
  );
});
