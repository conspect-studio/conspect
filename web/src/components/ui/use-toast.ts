// A tiny typed toast store + hook (no external state lib). Lives outside React
// so non-component code can enqueue a toast; components subscribe via the hook.
import { useSyncExternalStore } from "react";

/** A queued toast item. */
export interface ToastItem {
  /** Stable id for the toast instance. */
  readonly id: string;
  /** Short heading. */
  readonly title: string;
  /** Optional body. */
  readonly description?: string;
  /** Visual + semantic variant. */
  readonly variant?: "default" | "destructive";
}

/** Fields a caller supplies when raising a toast. */
export type ToastOptions = Omit<ToastItem, "id">;

type Listener = () => void;

let items: readonly ToastItem[] = [];
const listeners = new Set<Listener>();
let counter = 0;

function emit(): void {
  for (const listener of listeners) {
    listener();
  }
}

function subscribe(listener: Listener): () => void {
  listeners.add(listener);
  return (): void => {
    listeners.delete(listener);
  };
}

function getSnapshot(): readonly ToastItem[] {
  return items;
}

/** Enqueue a toast. Safe to call from anywhere, including outside React. */
export function toast(options: ToastOptions): string {
  counter += 1;
  const id = `toast-${String(counter)}`;
  const item: ToastItem = {
    id,
    title: options.title,
    ...(options.description !== undefined
      ? { description: options.description }
      : {}),
    ...(options.variant !== undefined ? { variant: options.variant } : {}),
  };
  items = [...items, item];
  emit();
  return id;
}

/** Remove a toast (called when the Radix toast finishes closing). */
export function dismissToast(id: string): void {
  items = items.filter((item) => item.id !== id);
  emit();
}

/** Subscribe a component to the current toast list. */
export function useToasts(): readonly ToastItem[] {
  return useSyncExternalStore(subscribe, getSnapshot, getSnapshot);
}
