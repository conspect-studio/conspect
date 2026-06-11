// shadcn/ui Label — Radix Label primitive (associates with a control for a11y).
import { forwardRef } from "react";
import type { ComponentPropsWithoutRef, ComponentRef } from "react";
import * as LabelPrimitive from "@radix-ui/react-label";

import { cn } from "../../lib/utils";

/** An accessible form label bound to its control. */
export const Label = forwardRef<
  ComponentRef<typeof LabelPrimitive.Root>,
  ComponentPropsWithoutRef<typeof LabelPrimitive.Root>
>(function Label({ className, ...props }, ref) {
  return (
    <LabelPrimitive.Root
      ref={ref}
      className={cn(
        "text-sm font-medium leading-none peer-disabled:cursor-not-allowed peer-disabled:opacity-70",
        className,
      )}
      {...props}
    />
  );
});
