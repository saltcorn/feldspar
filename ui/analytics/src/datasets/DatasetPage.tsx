// `#/datasets/<id>` (analytics TODO A1.17, A1.21): one dataset in the Dataset
// editor — where the front page's list of datasets and the admin UI's "Edit in
// Analytics" lead. Back goes to the front page.

import { useState } from "react";

import { useT } from "../i18n";
import { navigate } from "../router";
import { DatasetEditor } from "./DatasetEditor";

export function DatasetPage({ id }: { id: string }) {
  const { t } = useT();
  const [selected, setSelected] = useState<string | null>(null);
  return (
    <DatasetEditor
      id={id}
      selected={selected}
      onSelect={setSelected}
      onBack={() => navigate({ name: "home" })}
      backLabel={t("All datasets")}
    />
  );
}
