// MapLibre GL JS, as the Analytics UI loads it (analytics TODO A5.6): only by
// the map's own chunk (`MapView` is loaded lazily), so a workspace with no map
// does not download it.
//
// Its worker is the package's own module file, emitted beside the bundle by
// Vite's `?url` and named here. MapLibre would otherwise build it from a
// `blob:` URL, which the Analytics UI's Content-Security-Policy does not allow
// (`worker-src 'self'`, `sc-server`'s `security.rs`).

import { AttributionControl, Map as MapLibreMap, NavigationControl, setWorkerUrl } from "maplibre-gl";
import "maplibre-gl/dist/maplibre-gl.css";
import workerUrl from "maplibre-gl/dist/maplibre-gl-worker.mjs?url";

setWorkerUrl(workerUrl);

export { AttributionControl, MapLibreMap, NavigationControl };
