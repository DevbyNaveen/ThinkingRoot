/**
 * cyrb53 — 53-bit non-cryptographic hash. Two-32-bit-state mixing
 * keeps collisions sparse for short inputs (entity-name lists) while
 * staying allocation-free.
 *
 * Used as a stable fingerprint for the brain-graph layout cache so a
 * compile that doesn't change the entity set keeps reusing the
 * previously-converged node positions.  A truncated SHA would be
 * sturdier but pulling in a hash dep for one call site is overkill.
 */
export function cyrb53(input: string, seed = 0): string {
  let h1 = 0xdeadbeef ^ seed;
  let h2 = 0x41c6ce57 ^ seed;
  for (let i = 0; i < input.length; i++) {
    const ch = input.charCodeAt(i);
    h1 = Math.imul(h1 ^ ch, 2654435761);
    h2 = Math.imul(h2 ^ ch, 1597334677);
  }
  h1 = Math.imul(h1 ^ (h1 >>> 16), 2246822507) ^ Math.imul(h2 ^ (h2 >>> 13), 3266489909);
  h2 = Math.imul(h2 ^ (h2 >>> 16), 2246822507) ^ Math.imul(h1 ^ (h1 >>> 13), 3266489909);
  const hi = (h2 >>> 0) & 0x1fffff;
  const lo = h1 >>> 0;
  return (hi.toString(16).padStart(6, "0") + lo.toString(16).padStart(8, "0"));
}
