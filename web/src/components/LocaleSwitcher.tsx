// Locale switcher. Uses the accessible Radix Select; options are self-named.
import type { JSX } from "react";
import { useLingui } from "@lingui/react/macro";
import { Languages } from "lucide-react";

import { useLocale } from "../i18n/I18nProvider";
import { LOCALE_LABELS, SUPPORTED_LOCALES } from "../i18n/locales";
import type { Locale } from "../i18n/locales";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "./ui/select";

function isLocale(value: string): value is Locale {
  return (SUPPORTED_LOCALES as readonly string[]).includes(value);
}

/** A dropdown that switches the active UI locale (and writing direction). */
export function LocaleSwitcher(): JSX.Element {
  const { locale, setLocale } = useLocale();
  const { t } = useLingui();

  return (
    <Select
      value={locale}
      onValueChange={(value): void => {
        if (isLocale(value)) {
          setLocale(value);
        }
      }}
    >
      <SelectTrigger className="w-40" aria-label={t`Language`}>
        <Languages className="size-4 opacity-70" aria-hidden="true" />
        <SelectValue />
      </SelectTrigger>
      <SelectContent>
        {SUPPORTED_LOCALES.map((value) => (
          <SelectItem key={value} value={value} lang={value}>
            {LOCALE_LABELS[value]}
          </SelectItem>
        ))}
      </SelectContent>
    </Select>
  );
}
