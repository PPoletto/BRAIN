/// Pure-function helpers for the zoom-aware label visibility scheme in
/// GraphCanvas. Extracted here so the policy is unit-testable in
/// isolation from cytoscape — the GraphCanvas integration only has to
/// translate the helper's verdict into a `text-opacity` style.
///
/// Design intent (revised in 0.2.19 after Pascal's "zoom-out feels
/// empty" feedback on the 0.2.18 staffelung):
///   - Small vaults (< LABEL_DENSITY_THRESHOLD nodes) keep every label
///     visible at every zoom level. The whole staffelung exists to
///     make 200+-node graphs readable; on a fresh ~30-node vault we
///     do not want the user to see labels appear and disappear.
///   - Larger vaults staffel non-hub labels by zoom; hub nodes (top
///     HUB_PERCENTILE by degree) stay labeled at every zoom level
///     because they are the structural anchors a user navigates
///     overview by. The 0.2.18 version hid even hubs below a zoom
///     floor, which left the overview camera looking like an empty
///     dot soup. Now: hubs always; non-hubs only at zoom ≥
///     SHOW_ALL_LABELS_AT_OR_ABOVE_ZOOM.

/// Below this node count, label staffelung is bypassed entirely.
/// 80 is the empirical knee point where label collisions start to
/// dominate the canvas at default zoom (1.0) on a 1280×800 viewport.
export const LABEL_DENSITY_THRESHOLD = 80;

/// Top 15 % of nodes (by degree) qualify as "hubs" — the structural
/// anchors that stay labeled at overview zoom. On Pascal's
/// 362-node vault this picks the ~55 most-connected pages and
/// drops the long tail.
export const HUB_PERCENTILE = 0.15;

/// Zoom level at which non-hub labels appear. Below this we show
/// only the hubs (always); at or above this every label inside the
/// viewport is visible. Set to 0.7 so labels start appearing well
/// before the user is fully zoomed in — the 0.2.18 value (1.0) felt
/// too aggressive on dense vaults at default fit zoom.
export const SHOW_ALL_LABELS_AT_OR_ABOVE_ZOOM = 0.7;

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
  // Hubs stay labeled at every zoom level — they are the
  // structural anchors a user uses to find their way around in
  // the overview. (0.2.18 hid hubs below a zoom floor too, which
  // produced a visually empty overview.)
  if (degree >= hubThreshold) return 1;
  // Non-hub labels appear once the user has zoomed in past the
  // detail threshold — at that point the viewport contains a
  // small enough neighbourhood that label collisions are
  // unlikely.
  return zoom >= SHOW_ALL_LABELS_AT_OR_ABOVE_ZOOM ? 1 : 0;
}
