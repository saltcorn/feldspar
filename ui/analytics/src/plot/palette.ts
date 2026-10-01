// The colours plots are drawn in, light and dark (analytics TODO A2.7).
//
// The categorical slots are a validated palette taken whole: the order is what
// keeps adjacent series apart for colour-blind readers, so a series takes the
// slot of its value's place in the domain — colour follows the value, never its
// rank in what is shown — and a ninth value is drawn in the muted ink rather
// than in a generated hue. Magnitude is one hue from light to dark; a diverging
// scale (a correlation) is blue to red through a neutral grey.

/** What a chart is drawn with in one colour scheme. */
export type ChartPalette = {
  mode: "light" | "dark";
  /** Categorical slots, in their fixed order. */
  categorical: string[];
  /** A value past the eighth: drawn, but not told apart by colour. */
  other: string;
  /** Magnitude, from near nothing to most. */
  sequential: string[];
  /** Negative pole, neutral midpoint, positive pole. */
  diverging: string[];
  text: string;
  secondary: string;
  muted: string;
  grid: string;
  axis: string;
  surface: string;
};

const SEQUENTIAL = ["#cde2fb", "#9ec5f4", "#6da7ec", "#3987e5", "#256abf", "#184f95", "#0d366b"];

const LIGHT: ChartPalette = {
  mode: "light",
  categorical: ["#2a78d6", "#eb6834", "#1baf7a", "#eda100", "#e87ba4", "#008300", "#4a3aa7", "#e34948"],
  other: "#898781",
  sequential: SEQUENTIAL,
  diverging: ["#2a78d6", "#f0efec", "#e34948"],
  text: "#0b0b0b",
  secondary: "#52514e",
  muted: "#898781",
  grid: "#e1e0d9",
  axis: "#c3c2b7",
  surface: "#fcfcfb",
};

const DARK: ChartPalette = {
  mode: "dark",
  categorical: ["#3987e5", "#d95926", "#199e70", "#c98500", "#d55181", "#008300", "#9085e9", "#e66767"],
  other: "#898781",
  // Dark surfaces read magnitude from dark to light.
  sequential: [...SEQUENTIAL].reverse(),
  diverging: ["#3987e5", "#383835", "#e66767"],
  text: "#ffffff",
  secondary: "#c3c2b7",
  muted: "#898781",
  grid: "#2c2c2a",
  axis: "#383835",
  surface: "#1a1a19",
};

/** The palette of a colour scheme. */
export function chartPalette(mode: "light" | "dark"): ChartPalette {
  return mode === "dark" ? DARK : LIGHT;
}

/** The colour of the value at `index` of a discrete domain. */
export function slotColor(palette: ChartPalette, index: number): string {
  return index >= 0 && index < palette.categorical.length ? palette.categorical[index] : palette.other;
}
