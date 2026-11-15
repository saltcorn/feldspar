// Where an autosaving form's edits stand (`autosave.ts`), beside the page title.
//
// Shared by the screens that save as they are edited — an application's form
// and Settings — so "Saving…", "Saved" and "Not saved" read the same on both.

import Spinner from "react-bootstrap/Spinner";

import type { SaveState } from "./autosave";
import { useT } from "./i18n";

/** Where the form's edits stand, beside the page title. */
export function SaveStatus({ state }: { state: SaveState }) {
  const { t } = useT();
  switch (state.kind) {
    case "pending":
    case "saving":
      return (
        <span className="text-secondary small" role="status">
          <Spinner animation="border" size="sm" className="me-1" />
          {t("Saving…")}
        </span>
      );
    case "saved":
      return (
        <span className="text-success small" role="status">
          {t("Saved")}
        </span>
      );
    case "error":
      return (
        <span className="text-danger small" role="status">
          {t("Not saved")}
        </span>
      );
    default:
      return (
        <span className="text-secondary small" role="status">
          {t("Changes save as you edit")}
        </span>
      );
  }
}
