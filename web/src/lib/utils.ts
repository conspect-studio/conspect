import { clsx } from "clsx";
import type { ClassValue } from "clsx";
import { twMerge } from "tailwind-merge";

/**
 * Merge Tailwind class names (the shadcn/ui convention): `clsx` resolves
 * conditional/array class inputs and `tailwind-merge` de-duplicates conflicting
 * Tailwind utilities so the last one wins.
 */
export function cn(...inputs: readonly ClassValue[]): string {
  return twMerge(clsx(inputs));
}
