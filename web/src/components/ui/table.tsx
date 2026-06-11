// shadcn/ui Table primitives — real <table> semantics for AT (accessibility.md).
import { forwardRef } from "react";
import type {
  HTMLAttributes,
  TdHTMLAttributes,
  ThHTMLAttributes,
} from "react";

import { cn } from "../../lib/utils";

/** The table, wrapped for horizontal overflow. */
export const Table = forwardRef<
  HTMLTableElement,
  HTMLAttributes<HTMLTableElement>
>(function Table({ className, ...props }, ref) {
  return (
    <div className="relative w-full overflow-x-auto">
      <table
        ref={ref}
        className={cn("w-full caption-bottom text-sm", className)}
        {...props}
      />
    </div>
  );
});

/** Table head section. */
export const TableHeader = forwardRef<
  HTMLTableSectionElement,
  HTMLAttributes<HTMLTableSectionElement>
>(function TableHeader({ className, ...props }, ref) {
  return <thead ref={ref} className={cn("[&_tr]:border-b", className)} {...props} />;
});

/** Table body section. */
export const TableBody = forwardRef<
  HTMLTableSectionElement,
  HTMLAttributes<HTMLTableSectionElement>
>(function TableBody({ className, ...props }, ref) {
  return (
    <tbody
      ref={ref}
      className={cn("[&_tr:last-child]:border-0", className)}
      {...props}
    />
  );
});

/** A table row. */
export const TableRow = forwardRef<
  HTMLTableRowElement,
  HTMLAttributes<HTMLTableRowElement>
>(function TableRow({ className, ...props }, ref) {
  return (
    <tr
      ref={ref}
      className={cn(
        "border-b transition-colors hover:bg-muted/50 data-[state=selected]:bg-muted",
        className,
      )}
      {...props}
    />
  );
});

/** A column header cell (`<th scope="col">` by default). */
export const TableHead = forwardRef<
  HTMLTableCellElement,
  ThHTMLAttributes<HTMLTableCellElement>
>(function TableHead({ className, scope = "col", ...props }, ref) {
  return (
    <th
      ref={ref}
      scope={scope}
      className={cn(
        "h-10 px-2 text-start align-middle font-medium text-muted-foreground",
        className,
      )}
      {...props}
    />
  );
});

/** A data cell. */
export const TableCell = forwardRef<
  HTMLTableCellElement,
  TdHTMLAttributes<HTMLTableCellElement>
>(function TableCell({ className, ...props }, ref) {
  return (
    <td
      ref={ref}
      className={cn("p-2 align-middle", className)}
      {...props}
    />
  );
});

/** A table caption naming the table for AT. */
export const TableCaption = forwardRef<
  HTMLTableCaptionElement,
  HTMLAttributes<HTMLTableCaptionElement>
>(function TableCaption({ className, ...props }, ref) {
  return (
    <caption
      ref={ref}
      className={cn("mt-4 text-sm text-muted-foreground", className)}
      {...props}
    />
  );
});
