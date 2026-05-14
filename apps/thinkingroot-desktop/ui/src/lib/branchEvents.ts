/**
 * Branch SSE payloads mirror `thinkingroot_core::BranchEvent` JSON:
 * `#[serde(tag = "kind", rename_all = "snake_case")]` → lowercase tags.
 */
const BRANCH_EVENT_KINDS = new Set([
  "created",
  "merged",
  "abandoned",
  "redaction_updated",
  "permissions_updated",
  "contribute_bulk",
]);

/** True when `branch_list` should refetch after an aggregate SSE `event`. */
export function branchListShouldRefresh(event: unknown): boolean {
  if (!event || typeof event !== "object") return false;
  const obj = event as Record<string, unknown>;
  const k =
    typeof obj.kind === "string"
      ? obj.kind
      : typeof obj.type === "string"
        ? obj.type
        : undefined;
  if (k) {
    const lower = k.toLowerCase();
    if (BRANCH_EVENT_KINDS.has(lower)) return true;
  }
  const keys = Object.keys(obj);
  if (keys.length === 1 && BRANCH_EVENT_KINDS.has(keys[0]!.toLowerCase())) return true;
  return false;
}

/** When the branch list (and HEAD / `current` flags) should refetch. */
export function branchListNeedsRefetchFromEnvelope(env: {
  kind: string;
  event?: unknown;
}): boolean {
  if (
    env.kind === "head_changed" ||
    env.kind === "lagged" ||
    env.kind === "disconnected"
  ) {
    return true;
  }
  if (env.kind === "event") return branchListShouldRefresh(env.event);
  return false;
}
