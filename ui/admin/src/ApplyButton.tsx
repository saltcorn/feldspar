// The Apply button in a card's title row: for the settings a form holds until
// the admin says so, rather than saving them as they change.
//
// It sits at the top right of the card whose settings it applies, and it says
// at a glance whether there is anything to apply: **disabled and outlined** while
// the card matches what is stored, **primary** the moment it does not. A screen
// that autosaves everything else needs that difference to be visible, or an
// admin cannot tell an applied card from one still waiting.

import Button from "react-bootstrap/Button";
import Spinner from "react-bootstrap/Spinner";

import { useT } from "./i18n";
import { IconCheck } from "./icons";

/** How the button looks: primary and enabled with changes to apply; outlined
 * and disabled without, or while something is being saved. */
export function applyButtonLook(
  dirty: boolean,
  busy: boolean,
): { variant: "primary" | "outline-secondary"; disabled: boolean } {
  return { variant: dirty ? "primary" : "outline-secondary", disabled: !dirty || busy };
}

export function ApplyButton({
  dirty,
  busy = false,
  applying = false,
  form,
}: {
  /** The card holds changes that are not stored yet. */
  dirty: boolean;
  /** Something on the screen is saving: the button waits for it. */
  busy?: boolean;
  /** This card's own changes are being applied: shown with a spinner. */
  applying?: boolean;
  /** The `<form>` the button submits, by id, when the button is outside it —
   * a card's title row, above the form that holds the card's fields. */
  form?: string;
}) {
  const { t } = useT();
  const look = applyButtonLook(dirty, busy || applying);
  return (
    <Button
      type="submit"
      form={form}
      size="sm"
      variant={look.variant}
      disabled={look.disabled}
      title={dirty ? t("Apply the changes in this card") : t("No changes to apply")}
    >
      {applying ? (
        <Spinner animation="border" size="sm" className="me-1" />
      ) : (
        <IconCheck className="icon-2" />
      )}
      {applying ? t("Applying…") : t("Apply")}
    </Button>
  );
}
