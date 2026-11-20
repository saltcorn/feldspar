// The "+ New model", "+ New dataset", "+ New workspace" buttons on the front
// page's section headings: a plus icon before the label, the admin UI's
// convention for a button that creates something.

import Button from "react-bootstrap/Button";

import { IconPlus } from "../../admin/src/icons";

export function AddButton({
  label,
  onClick,
  className,
}: {
  /** What the button says after the icon: "New model". */
  label: string;
  onClick: () => void;
  className?: string;
}) {
  return (
    <Button size="sm" className={className} onClick={onClick}>
      <IconPlus className="icon-2" />
      {label}
    </Button>
  );
}
