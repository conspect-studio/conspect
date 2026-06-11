// shadcn/ui Dialog — Radix Dialog (focus trap + restore, Escape, aria wiring).
import { forwardRef } from "react";
import type {
  ComponentPropsWithoutRef,
  ComponentRef,
  HTMLAttributes,
  JSX,
} from "react";
import * as DialogPrimitive from "@radix-ui/react-dialog";
import { X } from "lucide-react";

import { cn } from "../../lib/utils";

export const Dialog = DialogPrimitive.Root;
export const DialogTrigger = DialogPrimitive.Trigger;
export const DialogClose = DialogPrimitive.Close;
export const DialogPortal = DialogPrimitive.Portal;

/** The dimmed backdrop behind the dialog. */
export const DialogOverlay = forwardRef<
  ComponentRef<typeof DialogPrimitive.Overlay>,
  ComponentPropsWithoutRef<typeof DialogPrimitive.Overlay>
>(function DialogOverlay({ className, ...props }, ref) {
  return (
    <DialogPrimitive.Overlay
      ref={ref}
      className={cn("fixed inset-0 z-50 bg-black/50", className)}
      {...props}
    />
  );
});

/** The dialog surface. Requires a `DialogTitle` for an accessible name. */
export const DialogContent = forwardRef<
  ComponentRef<typeof DialogPrimitive.Content>,
  ComponentPropsWithoutRef<typeof DialogPrimitive.Content>
>(function DialogContent({ className, children, ...props }, ref) {
  return (
    <DialogPortal>
      <DialogOverlay />
      <DialogPrimitive.Content
        ref={ref}
        className={cn(
          "fixed start-1/2 top-1/2 z-50 grid w-full max-w-lg -translate-x-1/2 -translate-y-1/2 gap-4 rounded-lg border bg-background p-6 shadow-lg rtl:translate-x-1/2",
          className,
        )}
        {...props}
      >
        {children}
        <DialogPrimitive.Close
          className="absolute end-4 top-4 rounded-sm opacity-70 transition-opacity hover:opacity-100 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
          aria-label="Close"
        >
          <X className="size-4" aria-hidden="true" />
        </DialogPrimitive.Close>
      </DialogPrimitive.Content>
    </DialogPortal>
  );
});

/** The dialog header region. */
export function DialogHeader({
  className,
  ...props
}: HTMLAttributes<HTMLDivElement>): JSX.Element {
  return (
    <div
      className={cn("flex flex-col gap-1.5 text-start", className)}
      {...props}
    />
  );
}

/** The dialog footer region (actions). */
export function DialogFooter({
  className,
  ...props
}: HTMLAttributes<HTMLDivElement>): JSX.Element {
  return (
    <div
      className={cn(
        "flex flex-col-reverse gap-2 sm:flex-row sm:justify-end",
        className,
      )}
      {...props}
    />
  );
}

/** The dialog title — supplies the accessible name (required). */
export const DialogTitle = forwardRef<
  ComponentRef<typeof DialogPrimitive.Title>,
  ComponentPropsWithoutRef<typeof DialogPrimitive.Title>
>(function DialogTitle({ className, ...props }, ref) {
  return (
    <DialogPrimitive.Title
      ref={ref}
      className={cn("text-lg font-semibold leading-none tracking-tight", className)}
      {...props}
    />
  );
});

/** The dialog description, tied via aria-describedby by Radix. */
export const DialogDescription = forwardRef<
  ComponentRef<typeof DialogPrimitive.Description>,
  ComponentPropsWithoutRef<typeof DialogPrimitive.Description>
>(function DialogDescription({ className, ...props }, ref) {
  return (
    <DialogPrimitive.Description
      ref={ref}
      className={cn("text-sm text-muted-foreground", className)}
      {...props}
    />
  );
});
