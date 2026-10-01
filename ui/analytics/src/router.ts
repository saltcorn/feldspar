// The Analytics UI's routes, on the URL's hash (analytics TODO A1.14).
//
// On the hash because the server serves one document for the whole bundle
// (`/analytics/`): a route is the part after `#`, so a reload or a bookmark
// comes back to the same screen without the server knowing any of them.
//
//   #/                        the front page: the datasets and the workspaces
//   #/w/<id>                  a workspace, opened on the state it was left in
//   #/datasets/<id>           a dataset in the Dataset editor — where the front
//                             page's datasets and "Edit in Analytics" lead
//   #/datasets/new?table=t    a new dataset, over `t` when one is named

/** Where the Analytics UI is. */
export type Route =
  | { name: "home" }
  | { name: "workspace"; id: string }
  | { name: "dataset"; id: string }
  | { name: "newDataset"; table: string | null }
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
    return { name: "dataset", id: parts[1] };
  }
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
      return `#/datasets/${encodeURIComponent(route.id)}`;
    case "newDataset":
      return route.table
        ? `#/datasets/new?table=${encodeURIComponent(route.table)}`
        : "#/datasets/new";
    case "notFound":
      return `#${route.path}`;
  }
}

/** Go to a route. */
export function navigate(route: Route): void {
  window.location.hash = routeHash(route);
}
