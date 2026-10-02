// The Analytics UI's routes, on the URL's hash (analytics TODO A1.14).
//
// On the hash because the server serves one document for the whole bundle
// (`/analytics/`): a route is the part after `#`, so a reload or a bookmark
// comes back to the same screen without the server knowing any of them.
//
//   #/                        the front page: the datasets and the workspaces
//   #/w/<id>                  a workspace, opened on the state it was left in
//   #/datasets/<id>           a dataset in the Dataset editor — where the front
//                             page's datasets and the model editor lead;
//                             `?back=<hash>` makes its Back return there
//   #/datasets/new?table=t    a new dataset, over `t` when one is named
//   #/models/<id>             a model in the model editor (A3.5); `?fit=<id>`
//                             selects one of its fits
//   #/models/new?dataset=d    a new model, on dataset `d` when one is named
//   #/models/compare?ids=a,b  the models' key outputs side by side
//   #/model-instances/<id>    a fit: the editor of its model, the fit selected
//                             — where the admin UI's old links lead

/** Where the Analytics UI is. */
export type Route =
  | { name: "home" }
  | { name: "workspace"; id: string }
  | { name: "dataset"; id: string; back?: string }
  | { name: "newDataset"; table: string | null }
  | { name: "model"; id: string; fit?: string }
  | { name: "newModel"; dataset: string | null }
  | { name: "compareModels"; ids: string[] }
  | { name: "fit"; id: string }
  | { name: "notFound"; path: string };

/** The route a hash names. */
export function parseRoute(hash: string): Route {
  const raw = hash.replace(/^#/, "");
  const [path, query = ""] = raw.split("?", 2);
  const params = new URLSearchParams(query);
  const parts = path.split("/").filter((p) => p !== "").map(decodeURIComponent);
  if (parts.length === 0) return { name: "home" };
  if (parts[0] === "w" && parts.length === 2) return { name: "workspace", id: parts[1] };
  if (parts[0] === "datasets" && parts.length === 2) {
    if (parts[1] === "new") return { name: "newDataset", table: params.get("table") };
    const back = params.get("back");
    return back ? { name: "dataset", id: parts[1], back } : { name: "dataset", id: parts[1] };
  }
  if (parts[0] === "models" && parts.length === 2) {
    if (parts[1] === "new") return { name: "newModel", dataset: params.get("dataset") };
    if (parts[1] === "compare") {
      const ids = (params.get("ids") ?? "").split(",").filter((id) => id !== "");
      return { name: "compareModels", ids };
    }
    const fit = params.get("fit");
    return fit ? { name: "model", id: parts[1], fit } : { name: "model", id: parts[1] };
  }
  if (parts[0] === "model-instances" && parts.length === 2) return { name: "fit", id: parts[1] };
  return { name: "notFound", path: `/${parts.join("/")}` };
}

/** The hash a route is at. */
export function routeHash(route: Route): string {
  switch (route.name) {
    case "home":
      return "#/";
    case "workspace":
      return `#/w/${encodeURIComponent(route.id)}`;
    case "dataset":
      return `#/datasets/${encodeURIComponent(route.id)}${query({ back: route.back })}`;
    case "newDataset":
      return route.table
        ? `#/datasets/new?table=${encodeURIComponent(route.table)}`
        : "#/datasets/new";
    case "model":
      return `#/models/${encodeURIComponent(route.id)}${query({ fit: route.fit })}`;
    case "newModel":
      return `#/models/new${query({ dataset: route.dataset })}`;
    case "compareModels":
      return `#/models/compare${query({ ids: route.ids.join(",") })}`;
    case "fit":
      return `#/model-instances/${encodeURIComponent(route.id)}`;
    case "notFound":
      return `#${route.path}`;
  }
}

/** A query string of the parameters that have a value, or `""`. */
function query(params: Record<string, string | null | undefined>): string {
  const present = Object.entries(params).filter(
    (entry): entry is [string, string] => typeof entry[1] === "string" && entry[1] !== "",
  );
  return present.length === 0 ? "" : `?${new URLSearchParams(present).toString()}`;
}

/** Go to a route. */
export function navigate(route: Route): void {
  window.location.hash = routeHash(route);
}
