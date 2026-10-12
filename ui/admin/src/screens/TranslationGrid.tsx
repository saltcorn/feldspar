// The translation grid: a row per message, one locale's column, a cell to type
// in. Shared by an application's Translations tab and the instance's shared
// Translations screen (TODO F6), so the two cannot drift apart.
//
// On an application's tab a row the application does not translate itself but
// the shared catalogue does shows the shared translation greyed, with
// **override**: clicking it copies the shared text into the cell as the start
// of the application's own.

import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import Table from "react-bootstrap/Table";

import { T, useT } from "../i18n";
import {
  inheritedEntry,
  placeholderProblem,
  singleForm,
  siteSummary,
  type MessageRow,
} from "../translations";

export function TranslationGrid({
  rows,
  locale,
  direction,
  edits,
  onEdit,
  showSites,
  empty,
}: {
  rows: MessageRow[];
  locale: string;
  direction: string | undefined;
  edits: Record<string, string>;
  onEdit: (key: string, value: string) => void;
  /** Whether to show the "Where" column — the shared catalogue has no source. */
  showSites: boolean;
  /** What an empty grid says. */
  empty: React.ReactNode;
}) {
  const { t } = useT();
  const columns = showSites ? 3 : 2;
  return (
    <Table hover responsive className="card-table table-vcenter">
      <thead>
        <tr>
          <th><T text="English" /></th>
          {showSites && <th><T text="Where" /></th>}
          <th>{locale}</th>
        </tr>
      </thead>
      <tbody>
        {rows.length === 0 && (
          <tr>
            <td colSpan={columns} className="text-muted">
              {empty}
            </td>
          </tr>
        )}
        {rows.map((row) => {
          const edited = Object.prototype.hasOwnProperty.call(edits, row.key);
          const single = singleForm(row.translations[locale]);
          const value = edited ? edits[row.key] : (single ?? "");
          const problem = placeholderProblem(row.key, value);
          const shared = inheritedEntry(row, locale);
          const sharedText = shared === undefined ? null : singleForm(shared);
          // Inherited and not being edited: the shared text, greyed.
          const showShared = sharedText !== null && shared !== undefined && !value.trim();
          return (
            <tr key={row.key} data-key={row.key}>
              <td>
                {row.source}
                {row.context && (
                  <div className="text-muted small">
                    <T text="context: {context}" args={{ context: row.context }} />
                  </div>
                )}
              </td>
              {showSites && <td className="text-muted small">{siteSummary(row.sites)}</td>}
              <td>
                {single === null ? (
                  // A plural entry has forms per CLDR category; a one-line box
                  // would save one of them over the rest.
                  <span className="text-muted">
                    <T text="Plural forms — edit the catalogue file" />
                  </span>
                ) : showShared && !edited ? (
                  <div className="d-flex align-items-center gap-2 inherited-translation">
                    <span className="text-muted fst-italic" title={t("From the shared catalogue")}>
                      {sharedText}
                    </span>
                    <span className="badge bg-secondary-lt"><T text="shared" /></span>
                    <Button
                      size="sm"
                      variant="link"
                      className="p-0"
                      onClick={() => onEdit(row.key, sharedText ?? "")}
                    >
                      <T text="override" />
                    </Button>
                  </div>
                ) : (
                  <>
                    <Form.Control
                      size="sm"
                      value={value}
                      isInvalid={Boolean(problem)}
                      dir={direction === "rtl" ? "rtl" : undefined}
                      aria-label={row.source}
                      placeholder={sharedText ?? undefined}
                      onChange={(event) => onEdit(row.key, event.target.value)}
                    />
                    {problem && <div className="invalid-feedback d-block">{problem}</div>}
                  </>
                )}
              </td>
            </tr>
          );
        })}
      </tbody>
    </Table>
  );
}
