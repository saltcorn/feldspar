// The Analytics UI's front page (analytics TODO A1.21, A3.5): the datasets,
// the models, and the workspaces. A dataset opens in the Dataset editor at
// `#/datasets/<id>`, a model in the model editor at `#/models/<id>`, a
// workspace in its frame at `#/w/<id>`.

import { DatasetList } from "./datasets/DatasetList";
import { ModelList } from "./models/ModelList";
import { usePane } from "./panes";
import { WorkspaceList } from "./workspaces/WorkspaceList";

export function Home() {
  const pane = usePane();
  return (
    <div className="an-page">
      <DatasetList onOpen={(id) => pane.go({ name: "dataset", id })} />
      <ModelList />
      <WorkspaceList />
    </div>
  );
}
