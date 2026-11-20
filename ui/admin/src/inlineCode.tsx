// Backticked names in text an agent wrote — a task's title and summary, a run's
// description — set as code. Not a Markdown renderer: the rest of the text
// stays as the agent wrote it (see `splitCodeBlocks` in agentChat.ts for why).

/** A run of text, or a `code` span in it. An unpaired backtick, or one whose
 * pair is on another line, is left as text. */
export function codeSpans(text: string): { code: boolean; text: string }[] {
  const parts: { code: boolean; text: string }[] = [];
  const span = /`([^`\n]+)`/g;
  let at = 0;
  let match: RegExpExecArray | null;
  while ((match = span.exec(text)) !== null) {
    if (match.index > at) parts.push({ code: false, text: text.slice(at, match.index) });
    parts.push({ code: true, text: match[1] });
    at = match.index + match[0].length;
  }
  if (at < text.length) parts.push({ code: false, text: text.slice(at) });
  return parts;
}

/** Text with its backticked names set as `<code>`. */
export function InlineCode({ text }: { text: string }) {
  return (
    <>
      {codeSpans(text).map((part, k) =>
        part.code ? <code key={k}>{part.text}</code> : <span key={k}>{part.text}</span>,
      )}
    </>
  );
}
