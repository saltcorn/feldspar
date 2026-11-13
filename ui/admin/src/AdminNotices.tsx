// The administrators' notices (TODO-type-hardening H8.6): what the server tells
// them once — the row-level-security upgrade report first — above every
// screen, until one of them dismisses it. Nothing at all when there are none,
// which is almost always.

import { useEffect, useState } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";

import { api } from "./api";
import type { ListAdminNoticesResponse } from "./client";
import { T } from "./i18n";

type Notice = ListAdminNoticesResponse[number];

export function AdminNotices() {
  const [notices, setNotices] = useState<Notice[]>([]);

  useEffect(() => {
    // Quietly nothing on failure: a notice is news, and a screen that could
    // not fetch it still works.
    api.listAdminNotices().then(setNotices, () => setNotices([]));
  }, []);

  const dismiss = async (notice: Notice) => {
    try {
      await api.dismissAdminNotice(notice.id);
    } finally {
      setNotices((current) => current.filter((n) => n.id !== notice.id));
    }
  };

  if (notices.length === 0) return null;
  return (
    <div className="container-xl mt-3">
      {notices.map((notice) => (
        <Alert key={notice.id} variant="warning">
          <div className="d-flex align-items-start gap-2">
            <div className="flex-grow-1">
              <h4 className="alert-title">{notice.title}</h4>
              <ul className="mb-0 small">
                {notice.lines.map((line, n) => (
                  <li key={n} className="text-break">{line}</li>
                ))}
              </ul>
            </div>
            <Button size="sm" variant="outline-secondary" onClick={() => void dismiss(notice)}>
              <T text="Dismiss" />
            </Button>
          </div>
        </Alert>
      ))}
    </div>
  );
}
