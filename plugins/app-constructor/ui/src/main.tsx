// The Plan & build screen's entry (TODO-app-constructor AC13.5): `mount` is
// what the admin calls with the element to render into and its host.
import { StrictMode } from "react";
import { createRoot } from "react-dom/client";

import type { Host } from "./host";
import { Screen } from "./Screen";
import { addStyles } from "./styles";

export function mount(element: HTMLElement, host: Host) {
  addStyles(element.ownerDocument);
  const root = createRoot(element);
  const render = (path: string) =>
    root.render(
      <StrictMode>
        <Screen host={{ ...host, path }} />
      </StrictMode>
    );
  render(host.path);
  return {
    update: render,
    unmount: () => root.unmount(),
  };
}
