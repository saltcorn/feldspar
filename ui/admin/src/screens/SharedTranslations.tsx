// Settings → Translations (shared): the instance's catalogue (TODO F6).
//
// Strings every application says — "Sign in", "Password", "Copy" — translated
// once here and served under each application's own catalogue: lookup is app →
// instance → key, merged on the server, so the applications' generated clients
// do not change. An application's own translation of a key wins.
//
// The same grid as an application's Translations tab. What differs is where
// the rows come from: there is no source to extract, so the keys are the ones
// the catalogue holds, and **Add a string** puts a new one in the grid. A row
// is kept once it is translated into at least one language; **Translate
// missing** fills the others. The languages are the installation's own
// (Settings → Localisation).

import { useEffect, useMemo, useState } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import Spinner from "react-bootstrap/Spinner";

import { api, errorMessage } from "../api";
import { navigate } from "../App";
import { T, useT } from "../i18n";
import { IconArrowLeft } from "../icons";
import { AlertBody, PageBody, PageHeader } from "../layout";
import {
  asTranslations,
  liveCoverage,
  missingKeys,
  newRow,
  saveBody,
  type MessageRow,
  type Translations as Payload,
} from "../translations";
import { TranslationGrid } from "./TranslationGrid";

export function SharedTranslations() {
  const { t } = useT();
  const [data, setData] = useState<Payload | null>(null);
  const [added, setAdded] = useState<MessageRow[]>([]);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [locale, setLocale] = useState<string | null>(null);
  const [edits, setEdits] = useState<Record<string, string>>({});
  const [busy, setBusy] = useState(false);
  const [adding, setAdding] = useState("");

  const load = async () => {
    try {
      const payload = asTranslations(await api.getInstanceTranslations());
      setData(payload);
      setEdits({});
      setAdded([]);
      setLocale((current) => {
        if (current && payload.locales.some((l) => l.locale === current)) return current;
        return payload.locales[0]?.locale ?? null;
      });
    } catch (err) {
      setLoadError(errorMessage(err, t("Could not load the shared translations.")));
    }
  };

  useEffect(() => {
    void load();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const rows = useMemo(() => [...(data?.messages ?? []), ...added], [data, added]);
  const current = data?.locales.find((l) => l.locale === locale) ?? null;
  const cover = useMemo(
    () => (locale ? liveCoverage(rows, locale, edits) : null),
    [rows, locale, edits],
  );
  const dirty = Object.keys(edits).length > 0;

  const save = async () => {
    if (!locale) return;
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      await api.saveInstanceTranslations(locale, { messages: saveBody(rows, locale, edits) });
      setNotice(t("Saved. Every application serves it on the next reload."));
      await load();
    } catch (err) {
      setError(errorMessage(err, t("Could not save the translations.")));
    } finally {
      setBusy(false);
    }
  };

  const fill = async () => {
    if (!locale) return;
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      const report = (await api.translateInstanceMissing(locale)) as {
        filled?: number;
        rejected?: { key: string; reason: string }[];
      };
      const rejected = report.rejected ?? [];
      setNotice(
        rejected.length === 0
          ? t("Translated {count} messages.", { count: report.filled ?? 0 })
          : t("Translated {count} messages; {bad} were rejected and left in English.", {
              count: report.filled ?? 0,
              bad: rejected.length,
            }),
      );
      if (rejected.length > 0) {
        setError(rejected.map((r) => `${r.key}: ${r.reason}`).join("\n"));
      }
      await load();
    } catch (err) {
      setError(errorMessage(err, t("Could not translate the missing messages.")));
    } finally {
      setBusy(false);
    }
  };

  const header = (
    <PageHeader
      pretitle={t("Settings")}
      title={t("Translations (shared)")}
      actions={
        <Button variant="outline-secondary" onClick={() => navigate("/settings")}>
          <IconArrowLeft className="icon-2" />
          <T text="Settings" />
        </Button>
      }
    />
  );

  if (loadError) {
    return (
      <>
        {header}
        <PageBody>
          <Alert variant="danger">{loadError}</Alert>
        </PageBody>
      </>
    );
  }
  if (!data) {
    return (
      <>
        {header}
        <PageBody>
          <Spinner animation="border" role="status" />
        </PageBody>
      </>
    );
  }

  const missing = locale ? missingKeys(rows, locale) : [];

  return (
    <>
      {header}
      <PageBody>
        {error && (
          <Alert variant="danger" dismissible onClose={() => setError(null)}>
            <AlertBody>{error}</AlertBody>
          </Alert>
        )}
        {notice && (
          <Alert variant="success" dismissible onClose={() => setNotice(null)}>
            <AlertBody>{notice}</AlertBody>
          </Alert>
        )}

        <div className="card mb-3">
          <div className="card-body">
            <p className="text-muted">
              <T text="Strings every application shares, translated once. An application serves these under its own translations: a string it translates itself wins, and one it does not is taken from here. No rebuild is needed." />
            </p>
            <div className="d-flex flex-wrap gap-2 align-items-center">
              {data.locales.map((l) => (
                <Button
                  key={l.locale}
                  size="sm"
                  variant={l.locale === locale ? "primary" : "outline-secondary"}
                  onClick={() => {
                    setLocale(l.locale);
                    setEdits({});
                  }}
                >
                  {l.locale} · {l.percent}%
                </Button>
              ))}
              {data.locales.length === 0 && (
                <span className="text-muted">
                  <T text="This installation serves one language. Add languages under Settings → Localisation to translate the shared strings." />
                </span>
              )}
            </div>
          </div>
        </div>

        {locale && (
          <div className="card mb-3">
            <div className="card-header d-flex justify-content-between align-items-center">
              <span>
                {cover && (
                  <T
                    text="{translated} of {total} messages translated into {locale} ({percent}%)"
                    args={{
                      translated: cover.translated,
                      total: cover.total,
                      percent: cover.percent,
                      locale,
                    }}
                  />
                )}
              </span>
              <span className="d-flex gap-2">
                <Button
                  size="sm"
                  variant="outline-primary"
                  disabled={busy || missing.length === 0 || dirty}
                  onClick={() => void fill()}
                >
                  <T text="Translate missing" />
                </Button>
                <Button size="sm" variant="primary" disabled={busy || !dirty} onClick={() => void save()}>
                  <T text="Save" />
                </Button>
              </span>
            </div>
            <TranslationGrid
              rows={rows}
              locale={locale}
              direction={current?.direction}
              edits={edits}
              onEdit={(key, value) => setEdits({ ...edits, [key]: value })}
              showSites={false}
              empty={<T text="No shared strings yet. Add one below." />}
            />
            <div className="card-footer">
              <Form
                className="d-flex gap-2"
                onSubmit={(event) => {
                  event.preventDefault();
                  const row = newRow(rows, adding);
                  if (!row) return;
                  setAdded([...added, row]);
                  setAdding("");
                }}
              >
                <Form.Control
                  style={{ maxWidth: "24rem" }}
                  value={adding}
                  placeholder={t("The English text, e.g. Sign in")}
                  aria-label={t("Add a string")}
                  onChange={(event) => setAdding(event.target.value)}
                />
                <Button type="submit" variant="outline-primary">
                  <T text="Add a string" />
                </Button>
              </Form>
              <div className="form-hint mt-2">
                <T text="A string is kept once it is translated into at least one language." />
              </div>
            </div>
          </div>
        )}
      </PageBody>
    </>
  );
}

/** The link on Settings → Localisation. */
export function SharedTranslationsPanel() {
  return (
    <div className="card mt-4">
      <div className="card-header"><T text="Translations (shared)" /></div>
      <div className="card-body">
        <p className="text-muted">
          <T text="Strings every application shares, such as Sign in or Password, translated once for all of them." />
        </p>
        <a className="btn btn-outline-primary" href="#/translations">
          <T text="Open the shared translations" />
        </a>
      </div>
    </div>
  );
}
