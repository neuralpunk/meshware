/** The make, spelled the way the make spells it. The wire carries
 *  whatever the driver said — `amd`, `nvidia` — and a lower-case make in
 *  a column of proper nouns reads as a typo. */
export function vendorLabel(v: string): string {
  const k = v.toLowerCase();
  if (k.includes("amd") || k.includes("radeon")) return "AMD";
  if (k.includes("nvidia")) return "NVIDIA";
  if (k.includes("intel")) return "Intel";
  return v;
}
