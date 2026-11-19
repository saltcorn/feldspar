// The Analytics UI's front page (analytics TODO A1.21, A3.5, A9.3): the
// workspaces, the datasets, and the models. A dataset opens in the Dataset
// editor at `#/datasets/<id>`, a model in the model editor at `#/models/<id>`,
// a workspace in its frame at `#/w/<id>`. A self-serve application's has no
// models: they are the unrestricted UI's.

import { DatasetList } from "./datasets/DatasetList";
import { ModelList } from "./models/ModelList";
import { usePane } from "./panes";
import { useShell } from "./shell";
import { WorkspaceList } from "./workspaces/WorkspaceList";

export function Home() {
  const pane = usePane();
  const { application } = useShell();
  return (
    <div className="an-page">
      <WorkspaceList />
      <DatasetList onOpen={(id) => pane.go({ name: "dataset", id })} />
      {!application && <ModelList />}
    </div>
  );
}
