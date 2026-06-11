// shadcn/ui Toast — Radix Toast primitives. Toasts announce via aria-live
// (Radix Provider) and are dismissable by keyboard (accessibility.md §4.1.3).
import { forwardRef } from "react";
import type { ComponentPropsWithoutRef, ComponentRef } from "react";
import * as ToastPrimitive from "@radix-ui/react-toast";
import { cva } from "class-variance-authority";
import type { VariantProps } from "class-variance-authority";
import { X } from "lucide-react";

import { cn } from "../../lib/utils";

export const ToastProvider = ToastPrimitive.Provider;

/** The fixed region toasts render into. */
export const ToastViewport = forwardRef<
  ComponentRef<typeof ToastPrimitive.Viewport>,
  ComponentPropsWithoutRef<typeof ToastPrimitive.Viewport>
>(function ToastViewport({ className, ...props }, ref) {
  return (
    <ToastPrimitive.Viewport
      ref={ref}
      className={cn(
        "fixed bottom-0 end-0 z-100 flex max-h-screen w-full flex-col gap-2 p-4 sm:max-w-sm",
        className,
      )}
      {...props}
    />
  );
});

const toastVariants = cva(
  "group pointer-events-auto relative flex w-full items-start justify-between gap-3 overflow-hidden rounded-md border p-4 shadow-lg",
  {
    variants: {
      variant: {
        default: "border bg-background text-foreground",
        destructive:
          "border-destructive bg-destructive text-destructive-foreground",
      },
    },
    defaultVariants: {
      variant: "default",
    },
  },
);

/** A single toast. */
export const Toast = forwardRef<
  ComponentRef<typeof ToastPrimitive.Root>,
  ComponentPropsWithoutRef<typeof ToastPrimitive.Root> &
    VariantProps<typeof toastVariants>
>(function Toast({ className, variant, ...props }, ref) {
  return (
    <ToastPrimitive.Root
      ref={ref}
      className={cn(toastVariants({ variant }), className)}
      {...props}
    />
  );
});

/** The toast title. */
export const ToastTitle = forwardRef<
  ComponentRef<typeof ToastPrimitive.Title>,
  ComponentPropsWithoutRef<typeof ToastPrimitive.Title>
>(function ToastTitle({ className, ...props }, ref) {
  return (
    <ToastPrimitive.Title
      ref={ref}
      className={cn("text-sm font-semibold", className)}
      {...props}
    />
  );
});

/** The toast description. */
export const ToastDescription = forwardRef<
  ComponentRef<typeof ToastPrimitive.Description>,
  ComponentPropsWithoutRef<typeof ToastPrimitive.Description>
>(function ToastDescription({ className, ...props }, ref) {
  return (
    <ToastPrimitive.Description
      ref={ref}
      className={cn("text-sm opacity-90", className)}
      {...props}
    />
  );
});

/** The dismiss control. */
export const ToastClose = forwardRef<
  ComponentRef<typeof ToastPrimitive.Close>,
  ComponentPropsWithoutRef<typeof ToastPrimitive.Close>
>(function ToastClose({ className, ...props }, ref) {
  return (
    <ToastPrimitive.Close
      ref={ref}
      className={cn(
        "rounded-md p-1 opacity-70 transition-opacity hover:opacity-100 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring",
        className,
      )}
      {...props}
    >
      <X className="size-4" aria-hidden="true" />
    </ToastPrimitive.Close>
  );
});
