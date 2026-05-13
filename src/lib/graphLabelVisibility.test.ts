import { describe, it, expect } from "vitest";
import {
  HIDE_ALL_LABELS_BELOW_ZOOM,
  SHOW_ALL_LABELS_AT_OR_ABOVE_ZOOM,
  LABEL_DENSITY_THRESHOLD,
  hubDegreeThreshold,
  decideLabelOpacity,
} from "./graphLabelVisibility";

describe("hubDegreeThreshold", () => {
  it("returns 0 for an empty graph so the policy is a no-op", () => {
    expect(hubDegreeThreshold([])).toBe(0);
  });

  it("picks the value at the 15-percentile rank on a uniform distribution", () => {
    // 100 nodes, degrees 1..100. Sorted desc: index 0 = 100,
    // index 15 = 85. We pick `sorted[floor(len × 0.15)]` so the
    // threshold is 85 — degrees ≥ 85 count as hubs (16 nodes,
    // marginally above the nominal 15 % but exactly what
    // `floor` gives us, and good enough for the policy).
    const degrees = Array.from({ length: 100 }, (_, i) => i + 1);
    expect(hubDegreeThreshold(degrees)).toBe(85);
  });

  it("handles a single-node graph by treating it as its own hub", () => {
    expect(hubDegreeThreshold([5])).toBe(5);
  });
});

describe("decideLabelOpacity", () => {
  const hub = 12; // assume threshold for these tests

  it("returns 1 for every node in a small graph regardless of zoom", () => {
    // Small graphs bypass staffelung — proves the "don't break tiny
    // vaults" contract.
    const small = LABEL_DENSITY_THRESHOLD - 1;
    expect(decideLabelOpacity({ nodeCount: small, zoom: 0.1, degree: 0, hubThreshold: hub })).toBe(1);
    expect(decideLabelOpacity({ nodeCount: small, zoom: 2.0, degree: 50, hubThreshold: hub })).toBe(1);
  });

  it("hides all labels in dense graphs when zoomed out below the threshold", () => {
    // Even a degree-100 hub disappears when the camera is too far
    // out — at this point labels would just be visual noise.
    expect(
      decideLabelOpacity({
        nodeCount: 300,
        zoom: HIDE_ALL_LABELS_BELOW_ZOOM - 0.01,
        degree: 100,
        hubThreshold: hub,
      }),
    ).toBe(0);
  });

  it("shows only hub labels in the middle zoom band of a dense graph", () => {
    // Mid-zoom: structural anchors stay labeled, long tail goes
    // dark. The user can still see the shape of the graph and read
    // the hub names without label collisions.
    const mid = (HIDE_ALL_LABELS_BELOW_ZOOM + SHOW_ALL_LABELS_AT_OR_ABOVE_ZOOM) / 2;
    expect(decideLabelOpacity({ nodeCount: 300, zoom: mid, degree: hub, hubThreshold: hub })).toBe(1);
    expect(decideLabelOpacity({ nodeCount: 300, zoom: mid, degree: hub + 5, hubThreshold: hub })).toBe(1);
    expect(decideLabelOpacity({ nodeCount: 300, zoom: mid, degree: hub - 1, hubThreshold: hub })).toBe(0);
  });

  it("shows every label in a dense graph once the user zooms past SHOW_ALL_AT", () => {
    // High-zoom: viewport contains a small neighbourhood, no
    // collisions, every label informative.
    expect(
      decideLabelOpacity({
        nodeCount: 300,
        zoom: SHOW_ALL_LABELS_AT_OR_ABOVE_ZOOM,
        degree: 0,
        hubThreshold: hub,
      }),
    ).toBe(1);
    expect(
      decideLabelOpacity({
        nodeCount: 300,
        zoom: SHOW_ALL_LABELS_AT_OR_ABOVE_ZOOM + 1.5,
        degree: 0,
        hubThreshold: hub,
      }),
    ).toBe(1);
  });
});
