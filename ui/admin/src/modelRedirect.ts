// Where the retired *Predictive models* screens' links go (analytics TODO
// A3.8): the Analytics UI's model editor, a page of its own under
// `/analytics/`. A bookmark, an old tutorial or a link in a trigger's
// description still lands on the model it named.
//
//   #/models                 → the Analytics front page, which lists them
//   #/models/new             → a new model in the editor
//   #/models/<id>            → that model in the editor
//   #/model-instances/<id>   → the editor, which finds the fit's model and
//                              selects the fit

/** The Analytics UI's address for an admin route that was a model screen, or
 * `null` for any other route. */
export function modelRedirect(path: string): string | null {
  const [route, query = ""] = path.split("?", 2);
  const parts = route.split("/").filter((p) => p !== "");
  if (parts[0] === "model-instances" && parts.length === 2) {
    return `/analytics/#/model-instances/${parts[1]}`;
  }
  if (parts[0] !== "models") return null;
  if (parts.length === 1) return "/analytics/#/";
  if (parts.length === 2) {
    return `/analytics/#/models/${parts[1]}${query ? `?${query}` : ""}`;
  }
  return null;
}
