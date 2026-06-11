// The app-level Toaster: renders the live toast queue inside a Radix provider.
import type { JSX } from "react";

import {
  Toast,
  ToastClose,
  ToastDescription,
  ToastProvider,
  ToastTitle,
  ToastViewport,
} from "./toast";
import { dismissToast, useToasts } from "./use-toast";

/** Mount once near the app root; renders queued toasts. */
export function Toaster(): JSX.Element {
  const toasts = useToasts();
  return (
    <ToastProvider swipeDirection="right">
      {toasts.map((item) => (
        <Toast
          key={item.id}
          variant={item.variant ?? "default"}
          onOpenChange={(open): void => {
            if (!open) {
              dismissToast(item.id);
            }
          }}
        >
          <div className="grid gap-1">
            <ToastTitle>{item.title}</ToastTitle>
            {item.description !== undefined ? (
              <ToastDescription>{item.description}</ToastDescription>
            ) : null}
          </div>
          <ToastClose />
        </Toast>
      ))}
      <ToastViewport />
    </ToastProvider>
  );
}
