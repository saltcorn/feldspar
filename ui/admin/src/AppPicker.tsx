// The application picker at the top of the sidebar's Applications section.
//
// It is drawn as a dropdown, not as one of the sidebar's submenus: a submenu
// that opens inline pushes the links under it down and reads as more
// navigation, when what this control does is change which application those
// links are about. So the toggle is a box — a small "Application" caption over
// the current application's name, with a caret — and the open menu floats over
// the links below it (`.app-picker-menu` in `admin.css`) rather than moving
// them. It is not a native `<select>`: one of those cannot be styled to sit in
// a dark Tabler sidebar, and it has no room for "All applications" and "New
// application" under the names.
//
// Folded to a rail, the toggle is just the icon and the menu is Tabler's own
// flyout beside it; the caption and the caret go with the link titles.
//
// This is the presentation only; `AppSidebar.tsx` owns which application is
// current and whether the menu is open.

import type { Ref } from "react";

import { IconApps, IconCheck, IconChevronDown } from "./icons";
import { T, useT } from "./i18n";

export type PickerApp = { id: string; name: string };

export function AppPicker({
  apps,
  current,
  open,
  path,
  listActive,
  folded,
  pickerRef,
  onToggle,
  onChoose,
  onClose,
}: {
  /** Null while the list is still loading. */
  apps: PickerApp[] | null;
  current: PickerApp | null;
  open: boolean;
  /** The route's path, for lighting up "All applications" / "New application". */
  path: string;
  /** On the applications list or the new-application form. */
  listActive: boolean;
  folded: boolean;
  pickerRef?: Ref<HTMLLIElement>;
  onToggle: () => void;
  onChoose: (app: PickerApp) => void;
  onClose: () => void;
}) {
  const { t } = useT();
  const label = current
    ? current.name
    : apps && apps.length === 0
      ? t("No applications")
      : t("Choose application");

  return (
    <li
      ref={pickerRef}
      className={`nav-item dropdown app-picker${listActive ? " active" : ""}${open ? " open" : ""}`}
      onKeyDown={(event) => {
        if (event.key === "Escape" && open) {
          event.stopPropagation();
          onClose();
        }
      }}
    >
      <button
        type="button"
        className="nav-link app-picker-toggle"
        aria-expanded={open}
        aria-haspopup="menu"
        aria-label={`${t("Application")}: ${label}`}
        title={folded ? label : undefined}
        onClick={onToggle}
      >
        <span className="nav-link-icon d-md-none d-lg-inline-block">
          <IconApps />
        </span>
        <span className="nav-link-title app-picker-text">
          <span className="app-picker-caption"><T text="Selected application" /></span>
          <span className={`app-picker-name${current ? "" : " text-secondary"}`}>{label}</span>
        </span>
        <span className="app-picker-caret sidebar-wide-only">
          <IconChevronDown />
        </span>
      </button>
      <div className={`dropdown-menu app-picker-menu${open ? " show" : ""}`} role="menu">
        {apps?.map((app) => {
          const chosen = app.id === current?.id;
          return (
            <button
              key={app.id}
              type="button"
              role="menuitemradio"
              aria-checked={chosen}
              className="dropdown-item"
              onClick={() => onChoose(app)}
            >
              <span className="app-picker-tick">{chosen && <IconCheck />}</span>
              <span className="app-picker-item-name">{app.name}</span>
            </button>
          );
        })}
        {apps?.length === 0 && (
          <span className="dropdown-item disabled"><T text="No applications yet" /></span>
        )}
        <div className="dropdown-divider" />
        <a
          className={`dropdown-item${path === "/applications" ? " active" : ""}`}
          href="#/applications"
          role="menuitem"
        >
          <span className="app-picker-tick" />
          <T text="All applications" />
        </a>
        <a
          className={`dropdown-item${path === "/applications/new" ? " active" : ""}`}
          href="#/applications/new"
          role="menuitem"
        >
          <span className="app-picker-tick" />
          <T text="New application" />
        </a>
      </div>
    </li>
  );
}
