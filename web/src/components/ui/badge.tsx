// shadcn/ui Badge — variant-driven status pill.
// Status variants pair a hue with REQUIRED text content (never color alone);
// a glyph is supplied by callers via children (accessibility.md §1.4.1).
import type { HTMLAttributes, JSX } from "react";
import { cva } from "class-variance-authority";
import type { VariantProps } from "class-variance-authority";

import { cn } from "../../lib/utils";

const badgeVariants = cva(
  "inline-flex items-center gap-1 rounded-md border px-2 py-0.5 text-xs font-medium transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring",
  {
    variants: {
      variant: {
        default: "border-transparent bg-primary text-primary-foreground",
        secondary: "border-transparent bg-secondary text-secondary-foreground",
        destructive:
          "border-transparent bg-destructive text-destructive-foreground",
        outline: "text-foreground",
        live: "border-status-live/40 bg-status-live/15 text-foreground",
        stale: "border-status-stale/40 bg-status-stale/15 text-foreground",
        reconnecting:
          "border-status-reconnecting/40 bg-status-reconnecting/15 text-foreground",
        nosignal:
          "border-status-nosignal/40 bg-status-nosignal/15 text-foreground",
        offline: "border-status-offline/40 bg-status-offline/15 text-foreground",
      },
    },
    defaultVariants: {
      variant: "default",
    },
  },
);

/** Props for {@link Badge}. */
export interface BadgeProps
  extends HTMLAttributes<HTMLSpanElement>,
    VariantProps<typeof badgeVariants> {}

/** A small status/label pill. */
export function Badge({ className, variant, ...props }: BadgeProps): JSX.Element {
  return (
    <span className={cn(badgeVariants({ variant }), className)} {...props} />
  );
}

export { badgeVariants };
