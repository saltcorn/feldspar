// The Analytics UI's front page (analytics TODO A1.21): the datasets, and
// below them the workspaces. A dataset opens in the Dataset editor at
// `#/datasets/<id>`; a workspace opens in its frame at `#/w/<id>`.

import { DatasetList } from "./datasets/DatasetList";
import { navigate } from "./router";
import { WorkspaceList } from "./workspaces/WorkspaceList";

export function Home() {
  return (
    <div className="an-page">
      <DatasetList onOpen={(id) => navigate({ name: "dataset", id })} />
      <WorkspaceList />
    </div>
  );
}
