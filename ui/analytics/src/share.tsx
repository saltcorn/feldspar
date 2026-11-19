// Sharing a dataset or a workspace with a role (analytics TODO A9.1): a select
// offered to whoever may change it — on a workspace's row, and under a
// dataset's name in the Dataset editor. Shared with a role, a thing is
// seen by every user of that role or a more privileged one, as a table's read
// role admits them; "Not shared" leaves it to its owner (and the admin).

import { useState } from "react";
import Form from "react-bootstrap/Form";

import { useT } from "./i18n";
import { useShell, type RoleInfo } from "./shell";

/** The roles a thing may be shared with: every role but the admin's, who sees
 * everything anyway, most privileged first. */
export function shareableRoles(roles: RoleInfo[]): RoleInfo[] {
  return roles.filter((r) => r.role > 1).sort((a, b) => a.role - b.role);
}

export function ShareSelect({
  value,
  label,
  onShare,
  wide = false,
}: {
  value: number | null | undefined;
  /** What is shared, for the control's accessible name. */
  label: string;
  onShare: (role: number | null) => Promise<void>;
  /** Full size and as wide as its container, like a form's own fields, rather
   * than small and as wide as its widest choice. */
  wide?: boolean;
}) {
  const { t } = useT();
  const { roles } = useShell();
  const [busy, setBusy] = useState(false);
  const choices = shareableRoles(roles);
  return (
    <Form.Select
      size={wide ? undefined : "sm"}
      className={wide ? "w-100" : "d-inline-block w-auto"}
      aria-label={t("Share {name} with", { name: label })}
      disabled={busy}
      value={value == null ? "" : String(value)}
      onChange={async (e) => {
        setBusy(true);
        try {
          await onShare(e.target.value === "" ? null : Number(e.target.value));
        } finally {
          setBusy(false);
        }
      }}
    >
      <option value="">{t("Not shared")}</option>
      {choices.map((r) => (
        <option key={r.role} value={r.role}>
          {t("Shared with {role}", { role: r.name })}
        </option>
      ))}
    </Form.Select>
  );
}
