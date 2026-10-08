// Reference layers (analytics TODO A5.11): tile and map services drawn under
// the data for context — satellite imagery, a cadastral map. They are not
// datasets, so nothing can be selected, styled or analysed in them.
//
// The Analytics UI may load images only from the hosts Settings → Maps names
// (its Content-Security-Policy), so a service on another host is said to be
// blocked, with a button that adds its host there. The policy is the page's,
// so the page is reloaded for it to apply.

import { useState } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";

import { api, errorMessage } from "../api";
import { T, useT } from "../i18n";
import type { ReferenceDraft, ReferenceLayer, ReferenceService } from "./spec";
import { hostAllowed, originOf } from "./workspace";

type Draft = { name: string; kind: ReferenceService["kind"]; url: string; layers: string; attribution: string };

const EMPTY: Draft = { name: "", kind: "tiles", url: "", layers: "", attribution: "" };

/** A draft's problem, as a sentence; `null` when it can be added. */
export function draftProblem(d: Draft, t: (s: string, args?: Record<string, string>) => string): string | null {
  if (d.name.trim() === "") return t("Give the layer a name.");
  if (!originOf(d.url)) return t("The address is an http or https URL.");
  if (d.kind === "tiles" && !/\{z\}/.test(d.url) && !/\{quadkey\}|\{bbox-epsg-3857\}/.test(d.url)) {
    return t("A tile address has {z}, {x} and {y} where the tile's zoom, column and row go.");
  }
  if (d.kind === "wms" && d.layers.trim() === "") return t("Name the service's layers to draw.");
  return null;
}

/** The reference layer a draft makes. */
export function referenceOf(d: Draft): ReferenceDraft {
  const base = { name: d.name.trim(), ...(d.attribution.trim() ? { attribution: d.attribution.trim() } : {}) };
  switch (d.kind) {
    case "wms":
      return { ...base, kind: "wms", url: d.url.trim(), layers: d.layers.trim() };
    case "arcgis":
      return { ...base, kind: "arcgis", url: d.url.trim() };
    default:
      return { ...base, kind: "tiles", url: d.url.trim() };
  }
}

export function ReferenceLayers({
  layers,
  hosts,
  onAdd,
  onChange,
  onRemove,
  onHostsChanged,
}: {
  layers: ReferenceLayer[];
  /** The origins a map may load from. */
  hosts: string[];
  onAdd: (ref: ReferenceDraft) => void;
  onChange: (id: string, change: (r: ReferenceLayer) => ReferenceLayer) => void;
  onRemove: (id: string) => void;
  onHostsChanged: (hosts: string[]) => void;
}) {
  const { t } = useT();
  const [draft, setDraft] = useState<Draft | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [allowed, setAllowed] = useState<string | null>(null);

  const allow = async (url: string) => {
    setError(null);
    try {
      const settings = await api.allowMapHost({ url });
      onHostsChanged(settings.hosts);
      setAllowed(originOf(url));
    } catch (err) {
      setError(errorMessage(err, t("The host could not be allowed.")));
    }
  };

  const problem = draft ? draftProblem(draft, t) : null;
  return (
    <div className="an-reference">
      {layers.length === 0 && !draft && (
        <p className="small text-secondary mb-1">
          <T text="Tile or map services drawn under the layers for context." />
        </p>
      )}
      {[...layers].reverse().map((r) => {
        const blocked = !hostAllowed(r.url, hosts);
        return (
          <div key={r.id} className="an-reference-row">
            <div className="d-flex align-items-center gap-2">
              <Form.Check
                aria-label={t("Show {name}", { name: r.name })}
                checked={r.visible !== false}
                onChange={(e) =>
                  onChange(r.id, (x) => {
                    const next = { ...x };
                    if (e.target.checked) delete next.visible;
                    else next.visible = false;
                    return next;
                  })
                }
              />
              <span className="text-truncate flex-grow-1" title={r.url}>
                {r.name}
              </span>
              <Button size="sm" variant="link" className="p-0 text-danger" onClick={() => onRemove(r.id)}>
                <T text="Remove" />
              </Button>
            </div>
            <Form.Range
              aria-label={t("Opacity of {name}", { name: r.name })}
              min={0}
              max={100}
              value={Math.round((r.opacity ?? 1) * 100)}
              onChange={(e) => onChange(r.id, (x) => ({ ...x, opacity: Number(e.target.value) / 100 }))}
            />
            {blocked && (
              <Alert variant="warning" className="small p-2 mb-1">
                {t("{host} is not among the map hosts in Settings → Maps, so its images are not loaded.", {
                  host: originOf(r.url) ?? r.url,
                })}
                <div className="mt-1">
                  <Button size="sm" variant="outline-secondary" onClick={() => void allow(r.url)}>
                    <T text="Allow it" />
                  </Button>
                </div>
              </Alert>
            )}
          </div>
        );
      })}
      {allowed && (
        <Alert variant="info" className="small p-2" dismissible onClose={() => setAllowed(null)}>
          {t("{host} is allowed. Reload the page to load its images.", { host: allowed })}
          <div className="mt-1">
            <Button size="sm" variant="outline-secondary" onClick={() => window.location.reload()}>
              <T text="Reload" />
            </Button>
          </div>
        </Alert>
      )}
      {error && <Alert variant="danger" className="small p-2">{error}</Alert>}
      {draft ? (
        <div className="an-reference-form">
          <Form.Control
            size="sm"
            className="mb-1"
            placeholder={t("Name")}
            aria-label={t("Name")}
            value={draft.name}
            onChange={(e) => setDraft({ ...draft, name: e.target.value })}
          />
          <Form.Select
            size="sm"
            className="mb-1"
            aria-label={t("Kind of service")}
            value={draft.kind}
            onChange={(e) => setDraft({ ...draft, kind: e.target.value as Draft["kind"] })}
          >
            <option value="tiles">{t("Tiles ({z}/{x}/{y})")}</option>
            <option value="wms">{t("Web Map Service (WMS)")}</option>
            <option value="arcgis">{t("ArcGIS map service")}</option>
          </Form.Select>
          <Form.Control
            size="sm"
            className="mb-1 font-monospace"
            placeholder={
              draft.kind === "tiles"
                ? "https://tile.openstreetmap.org/{z}/{x}/{y}.png"
                : draft.kind === "wms"
                  ? "https://example.com/wms"
                  : "https://example.com/arcgis/rest/services/…/MapServer"
            }
            aria-label={t("Address")}
            value={draft.url}
            onChange={(e) => setDraft({ ...draft, url: e.target.value })}
          />
          {draft.kind === "wms" && (
            <Form.Control
              size="sm"
              className="mb-1"
              placeholder={t("Layers, comma-separated")}
              aria-label={t("Layers")}
              value={draft.layers}
              onChange={(e) => setDraft({ ...draft, layers: e.target.value })}
            />
          )}
          <Form.Control
            size="sm"
            className="mb-1"
            placeholder={t("Attribution (the credit the provider asks for)")}
            aria-label={t("Attribution")}
            value={draft.attribution}
            onChange={(e) => setDraft({ ...draft, attribution: e.target.value })}
          />
          {problem && draft.url !== "" && <div className="small text-secondary mb-1">{problem}</div>}
          <div className="d-flex gap-2">
            <Button
              size="sm"
              disabled={problem !== null}
              onClick={() => {
                onAdd(referenceOf(draft));
                setDraft(null);
              }}
            >
              <T text="Add" />
            </Button>
            <Button size="sm" variant="secondary" onClick={() => setDraft(null)}>
              <T text="Cancel" />
            </Button>
          </div>
        </div>
      ) : (
        <Button size="sm" variant="outline-secondary" onClick={() => setDraft(EMPTY)}>
          <T text="Add reference layer" />
        </Button>
      )}
    </div>
  );
}
