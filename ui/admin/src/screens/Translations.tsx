// An application's Translations tab (design §16.1, task 4.4).
//
// Type **B** strings: the admin's own, written while they built the
// application. The screen is one table — a row per message the application's
// source says, a column per locale it serves — and everything around it exists
// to make that table trustworthy:
//
// - **The keys come from the source, not from a record of it.** The server runs
//   the same tree-sitter extraction `feldspar i18n extract` runs, over the
//   application's own file store, so a message added five minutes ago is here
//   and a message deleted five minutes ago is not.
// - **Coverage is a number, not a gate.** It moves as cells are typed, because
//   the alternative is a figure that is always one save behind.
// - **Translate missing** fills a column through the configured LLM, and the
//   machine checks the placeholders (D9). A rejected translation is named here
//   rather than being served to a customer.
// - **Orphans are shown and never deleted.** A key the source no longer uses is
//   listed under the grid; the server puts it back on every save.
// - **Unwrapped literals** are the other half of the same parse: a bare English
//   sentence nothing wraps can never be translated, so the screen says where it
//   is.
// - **Shared strings show through.** A key the instance's shared catalogue
//   translates (Settings → Translations (shared), TODO F6) and this
//   application does not is shown greyed, with **override**.

import { useEffect, useMemo, useState } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import Spinner from "react-bootstrap/Spinner";

import { api, errorMessage } from "../api";
import { navigate } from "../App";
import { IconArrowLeft } from "../icons";
import { AlertBody, PageBody, PageHeader } from "../layout";
import {
  asTranslations,
  liveCoverage,
  missingKeys,
  saveBody,
  type Translations as Payload,
} from "../translations";
import type { AppItem } from "../views";
import { ApplicationTabs } from "./ApplicationViews";
import { TranslationGrid } from "./TranslationGrid";
import { T, useT } from "../i18n";

export function ApplicationTranslations({ appId }: { appId: string }) {
  const { t } = useT();
  const [app, setApp] = useState<AppItem | null>(null);
  const [data, setData] = useState<Payload | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [locale, setLocale] = useState<string | null>(null);
  const [edits, setEdits] = useState<Record<string, string>>({});
  const [busy, setBusy] = useState(false);
  const [adding, setAdding] = useState("");

  const load = async () => {
    try {
      const found = (await api.listApplications()).find((a) => a.id === appId);
      if (!found) {
        setLoadError(t("That application no longer exists."));
        return;
      }
      setApp(found);
      const payload = asTranslations(await api.getTranslations(appId));
      setData(payload);
      setEdits({});
      setLocale((current) => {
        if (current && payload.locales.some((l) => l.locale === current)) return current;
        return payload.locales[0]?.locale ?? null;
      });
    } catch (err) {
      setLoadError(errorMessage(err, t("Could not load the application's translations.")));
    }
  };

  useEffect(() => {
    void load();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [appId]);

  const rows = data?.messages ?? [];
  const current = data?.locales.find((l) => l.locale === locale) ?? null;
  const cover = useMemo(
    () => (locale ? liveCoverage(rows, locale, edits) : null),
    [rows, locale, edits],
  );
  const dirty = Object.keys(edits).length > 0;

  const enable = async (tags: string[], fallback: string | null) => {
    setBusy(true);
    setError(null);
    try {
      await api.setApplicationLocales(appId, {
        locales: tags,
        default_locale: fallback ?? undefined,
      });
      await load();
    } catch (err) {
      setError(errorMessage(err, t("Could not change the application's locales.")));
    } finally {
      setBusy(false);
    }
  };

  const save = async () => {
    if (!locale || !data) return;
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      await api.saveTranslations(appId, locale, { messages: saveBody(rows, locale, edits) });
      setNotice(t("Saved. The running application serves it on the next reload."));
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
      const report = (await api.translateMissing(appId, locale)) as {
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
      pretitle={t("Application")}
      title={app?.name ?? t("Application")}
      actions={
        <Button variant="outline-secondary" onClick={() => navigate("/applications")}>
          <IconArrowLeft className="icon-2" />
          <T text="Applications" />
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
  if (!app || !data) {
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
        <ApplicationTabs app={app} active="translations" />

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
            <h3 className="card-title"><T text="Locales" /></h3>
            <p className="text-muted">
              <T
                text="The catalogues are kept in {store}, and served to the running application — a fixed translation is live on the next reload, with no rebuild."
                values={{ store: <code>{data.store}</code> }}
              />{" "}
              <T
                text="Strings this application does not translate are served from the {shared}, shown greyed below."
                values={{ shared: <a href="#/translations">{t("shared catalogue")}</a> }}
              />
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
                  <T text="This application serves one language. Add a locale to translate it." />
                </span>
              )}
            </div>
            <Form
              className="d-flex gap-2 mt-3"
              onSubmit={(event) => {
                event.preventDefault();
                const tag = adding.trim();
                if (!tag) return;
                setAdding("");
                void enable(
                  [...data.locales.map((l) => l.locale), tag],
                  data.default_locale ?? tag,
                );
              }}
            >
              <Form.Control
                style={{ maxWidth: "12rem" }}
                value={adding}
                placeholder={t("A locale tag, e.g. fr")}
                aria-label={t("A locale tag, e.g. fr")}
                onChange={(event) => setAdding(event.target.value)}
              />
              <Button type="submit" variant="outline-primary" disabled={busy}>
                <T text="Add locale" />
              </Button>
              {locale && (
                <Button
                  variant="outline-danger"
                  disabled={busy}
                  onClick={() => {
                    if (
                      !window.confirm(
                        t("Stop serving {locale}? Its catalogue is kept, so turning it back on costs nothing.", {
                          locale,
                        }),
                      )
                    ) {
                      return;
                    }
                    const left = data.locales.map((l) => l.locale).filter((l) => l !== locale);
                    void enable(
                      left,
                      data.default_locale === locale ? (left[0] ?? null) : data.default_locale,
                    );
                  }}
                >
                  <T text="Stop serving this locale" />
                </Button>
              )}
            </Form>
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
                  disabled={busy || missing.length === 0}
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
              showSites
              empty={<T text="Nothing to translate: this application's source has no t() calls yet." />}
            />
          </div>
        )}

        {current && current.orphans.length > 0 && (
          <div className="card mb-3">
            <div className="card-body">
              <h3 className="card-title"><T text="No longer used" /></h3>
              <p className="text-muted">
                <T text="These are translated but the source no longer says them. They are kept, never deleted: the source may be mid-edit, and throwing away a translation to tidy a list is not a trade worth making." />
              </p>
              <ul className="mb-0">
                {current.orphans.map((key) => (
                  <li key={key}><code>{key}</code></li>
                ))}
              </ul>
            </div>
          </div>
        )}

        {data.problems.length > 0 && (
          <div className="card mb-3">
            <div className="card-body">
              <h3 className="card-title text-danger"><T text="Messages that cannot be extracted" /></h3>
              <p className="text-muted">
                <T text="A t() call whose message is not a literal can never reach a catalogue, so it will stay English in every language." />
              </p>
              <ul className="mb-0">
                {data.problems.map((p, index) => (
                  <li key={index}>
                    <code>{p.file}:{p.line}</code> — {p.message}
                  </li>
                ))}
              </ul>
            </div>
          </div>
        )}

        {data.unwrapped.length > 0 && (
          <div className="card">
            <div className="card-body">
              <h3 className="card-title"><T text="Not wrapped in t()" /></h3>
              <p className="text-muted">
                <T
                  text="{count} English literals nobody wrapped. A literal that no t() wraps is invisible to the extractor and can never be translated."
                  args={{ count: data.unwrapped.length }}
                />
              </p>
              <ul className="mb-0">
                {data.unwrapped.slice(0, 50).map((u, index) => (
                  <li key={index}>
                    <code>{u.file}:{u.line}</code> — {u.text}
                  </li>
                ))}
              </ul>
            </div>
          </div>
        )}
      </PageBody>
    </>
  );
}
