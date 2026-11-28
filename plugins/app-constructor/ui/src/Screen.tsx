// The Plan & build screen (TODO-app-constructor AC13.5): the construction of
// the application the admin mounted it for, on the tab or phase page that
// `host.path` names. Each path is rendered afresh, as the admin's own route
// was (keyed by the path); the construction is loaded again for it.
import Alert from "react-bootstrap/Alert";

import { LinksProvider } from "./bits";
import { Construct } from "./Construct";
import { constructRoute } from "./construction";
import type { Host } from "./host";
import { T } from "./i18n";

export function Screen({ host }: { host: Host }) {
  const route = constructRoute(host.path);
  return (
    <LinksProvider value={{ href: host.href }}>
      {!host.app ? (
        <Alert variant="secondary">
          <T text="Plan & build is opened from an application." />
        </Alert>
      ) : !route ? (
        <Alert variant="secondary">
          <T text="There is no such page of Plan & build." />{" "}
          <a href={host.href("")}>
            <T text="Open Plan & build" />
          </a>
        </Alert>
      ) : (
        <Construct
          key={host.path}
          host={host}
          tab={route.tab}
          phaseId={route.phaseId}
          feedbackPhase={route.feedbackPhase}
        />
      )}
    </LinksProvider>
  );
}
