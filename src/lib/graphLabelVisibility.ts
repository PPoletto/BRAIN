/// Pure-function helpers for the zoom-aware label visibility scheme in
/// GraphCanvas. Extracted here so the policy is unit-testable in
/// isolation from cytoscape — the GraphCanvas integration only has to
/// translate the helper's verdict into a `text-opacity` style.
///
/// Design intent:
///   - Small vaults (< LABEL_DENSITY_THRESHOLD nodes) keep every label
///     visible at every zoom level. The whole staffelung exists to
///     make 200+-node graphs readable; on a fresh ~30-node vault we
///     do not want the user to see labels appear and disappear.
///   - Larger vaults staffel labels by zoom:
///       zoom < HIDE_ALL_BELOW          → no labels (overview, dots only)
///       HIDE_ALL_BELOW ≤ zoom < SHOW_ALL_AT
///                                       → only "hub" nodes (top
///                                         HUB_PERCENTILE by degree)
///                                         keep their labels — pulls
///                                         the structural anchors out
///                                         of the visual noise
///       zoom ≥ SHOW_ALL_AT             → every label visible

/// Below this node count, label staffelung is bypassed entirely.
/// 80 is the empirical knee point where label collisions start to
/// dominate the canvas at default zoom (1.0) on a 1280×800 viewport.
export const LABEL_DENSITY_THRESHOLD = 80;

/// Top 15 % of nodes (by degree) qualify as "hubs" — the structural
/// anchors that stay labeled at overview zoom. On Pascal's
/// 362-node vault this picks the ~55 most-connected pages and
/// drops the long tail.
export const HUB_PERCENTILE = 0.15;

/// Zoom-level boundaries for the label-staffelung. Values are
/// cytoscape `zoom` units — 1.0 ≈ "default fit", smaller numbers
/// are zoomed out further.
export const HIDE_ALL_LABELS_BELOW_ZOOM = 0.5;
export const SHOW_ALL_LABELS_AT_OR_ABOVE_ZOOM = 1.0;

/// Compute the degree value at which a node is considered a "hub"
/// (top HUB_PERCENTILE). The threshold is the degree of the node at
/// the percentile rank — i.e. any node with degree ≥ threshold is a
/// hub. Empty input → 0 (a degenerate graph has no hubs, fine).
export function hubDegreeThreshold(degrees: number[]): number {
  if (degrees.length === 0) return 0;
  // Sort descending; the value at index floor(len × percentile) is
  // the cutoff. We use Math.max(1, …) so a graph with very few
  // nodes still has at least one hub.
  const sorted = [...degrees].sort((a, b) => b - a);
  const idx = Math.max(0, Math.min(sorted.length - 1, Math.floor(sorted.length * HUB_PERCENTILE)));
  return sorted[idx];
}

/// Decide whether a single node's label should be visible right now.
/// Returns `1` (fully opaque) or `0` (hidden). Cytoscape's
/// `text-opacity` style takes a number in [0, 1] — we deliberately
/// stay binary so the user sees a clean "labels are here / labels
/// are gone" transition at the zoom boundaries rather than a fade.
/// Return type is `number` (not the literal `0 | 1`) so cytoscape's
/// style API consumes the value through its `(ele) => number`
/// overload rather than narrowing to a wrong sibling overload.
export function decideLabelOpacity(args: {
  nodeCount: number;
  zoom: number;
  degree: number;
  hubThreshold: number;
}): number {
  const { nodeCount, zoom, degree, hubThreshold } = args;
  // Small graphs: always show every label. The staffelung is a
  // density mitigation, not a default UX choice.
  if (nodeCount < LABEL_DENSITY_THRESHOLD) return 1;
  if (zoom < HIDE_ALL_LABELS_BELOW_ZOOM) return 0;
  if (zoom >= SHOW_ALL_LABELS_AT_OR_ABOVE_ZOOM) return 1;
  // Middle band: only hubs get labels.
  return degree >= hubThreshold ? 1 : 0;
}
