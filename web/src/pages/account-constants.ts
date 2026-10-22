// Claim-code constants + validators (§2.1/§2.4), kept in their own module so the
// page file exports only components (react-refresh) and so the portal and the
// machine validate identically.

/**
 * The fixed claim-code length (§2.4 — pinned; the portal and the machine
 * generate/validate to exactly this).
 */
export const CLAIM_CODE_LEN = 6;

/**
 * The ambiguity-free claim-code alphabet (§2.4). Excludes the visually ambiguous
 * glyphs `0`/`O`/`1`/`I`/`L` so a code is read back unambiguously. The exact
 * portal alphabet is an operator-confirm item (brief §14 O3); this is the
 * documented ambiguity-free set and is pinned by the test.
 */
export const CLAIM_CODE_CHARSET = "ABCDEFGHJKMNPQRSTUVWXYZ23456789";

/** Whether a single character is a member of the claim-code charset (case-insensitive). */
export function isClaimCodeChar(glyph: string): boolean {
  if (glyph.length !== 1) {
    return false;
  }
  return CLAIM_CODE_CHARSET.includes(glyph.toUpperCase());
}

/** Keep only valid claim-code glyphs, upper-cased, capped at the fixed length. */
export function canonicaliseClaimCode(raw: string): string {
  let out = "";
  for (const glyph of raw.toUpperCase()) {
    if (CLAIM_CODE_CHARSET.includes(glyph) && out.length < CLAIM_CODE_LEN) {
      out += glyph;
    }
  }
  return out;
}
