/// <reference types="vite/client" />

// Vite's ambient module declarations: `import.meta.glob` (the catalogues) and
// `?url` imports (the right-to-left stylesheet).

// Monaco's editor contributions (the program editor, `models/ProgramEditor.tsx`)
// are imported for their side effects and ship no declaration of their own.
declare module "monaco-editor/esm/vs/editor/editor.all.js";
