// The screen's texts, in English. The admin's own i18n is not an API a module
// can use, so this keeps its calls (`t(text, args)`, `<T text=… />`) and
// only fills in `{name}` placeholders. A placeholder with no argument stays as
// written, as the admin's does.

export type Args = Record<string, string | number>;

export function t(text: string, args: Args = {}): string {
  if (!text.includes("{")) return text;
  return text.replace(/\{([A-Za-z_][A-Za-z0-9_]*)\}/g, (whole, name: string) =>
    Object.prototype.hasOwnProperty.call(args, name)
      ? String(args[name])
      : whole
  );
}

export type Translate = typeof t;

/** As the admin's hook, so the ported components read the same. */
export function useT(): { t: Translate } {
  return { t };
}

export function T({ text, args }: { text: string; args?: Args }) {
  return <>{t(text, args)}</>;
}
