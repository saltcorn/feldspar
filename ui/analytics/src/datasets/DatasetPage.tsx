// `#/datasets/<id>` (analytics TODO A1.17, A1.21): one dataset in the Dataset
// editor — where the front page's list of datasets and the model editor lead.
// Back goes to the front page, or where `?back=` says it was opened from (the
// model editor, A3.5).

import { useState } from "react";

import { useT } from "../i18n";
import { navigate } from "../router";
import { DatasetEditor } from "./DatasetEditor";

export function DatasetPage({ id, back }: { id: string; back?: string }) {
  const { t } = useT();
  const [selected, setSelected] = useState<string | null>(null);
  const fromModel = back?.startsWith("#/models/") ?? false;
  return (
    <DatasetEditor
      id={id}
      selected={selected}
      onSelect={setSelected}
      onBack={() => (back ? (window.location.hash = back) : navigate({ name: "home" }))}
      backLabel={fromModel ? t("Back to the model") : t("All datasets")}
    />
  );
}
