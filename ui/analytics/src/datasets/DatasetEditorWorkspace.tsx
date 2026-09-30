// The Dataset editor workspace (analytics TODO A1.16, A1.17): the list of
// datasets until one is opened, then that dataset — remembering which one and
// which operation, so the workspace reopens where it was left.
//
// `DatasetEditorStandalone` is the same editor outside any workspace, at
// `#/datasets/<id>`: where the admin UI's "Edit in Analytics" goes.

import { useState } from "react";

import { useT } from "../i18n";
import { navigate } from "../router";
import type { WorkspaceProps } from "../workspaces/WorkspaceFrame";
import { DatasetEditor } from "./DatasetEditor";
import { DatasetList } from "./DatasetList";
import { backToList, openDataset, readEditorState, selectOperation } from "./editorState";

export function DatasetEditorWorkspace({ state, setState }: WorkspaceProps) {
  const { t } = useT();
  const { dataset, operation } = readEditorState(state);
  if (!dataset) {
    return <DatasetList onOpen={(id) => setState((s) => openDataset(s, id))} />;
  }
  return (
    <DatasetEditor
      key={dataset}
      id={dataset}
      selected={operation}
      onSelect={(op) => setState((s) => selectOperation(s, op))}
      onBack={() => setState(backToList)}
      backLabel={t("All datasets")}
    />
  );
}

export function DatasetEditorStandalone({ id }: { id: string }) {
  const { t } = useT();
  const [selected, setSelected] = useState<string | null>(null);
  return (
    <DatasetEditor
      id={id}
      selected={selected}
      onSelect={setSelected}
      onBack={() => navigate({ name: "home" })}
      backLabel={t("Workspaces")}
    />
  );
}
