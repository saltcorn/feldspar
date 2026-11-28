// The screen's styles, added to the admin's page once when the screen is
// first mounted: one stylesheet in the bundle, so the module serves one file.
import css from "./construct.css?inline";

const STYLE_ID = "feldspar-app-constructor-styles";

export function addStyles(doc: Document = document): void {
  if (doc.getElementById(STYLE_ID)) return;
  const style = doc.createElement("style");
  style.id = STYLE_ID;
  style.textContent = css;
  doc.head.appendChild(style);
}
