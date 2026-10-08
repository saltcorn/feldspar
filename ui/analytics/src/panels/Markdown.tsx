// Markdown drawn as React elements (analytics TODO A4.4), from the tree
// `markdown.ts` parses: never as HTML, so what is typed is never markup.

import { createElement, type ReactNode } from "react";

import { parseMarkdown, type Inline, type MdBlock } from "./markdown";

export function Markdown({ source, className }: { source: string; className?: string }) {
  return <div className={className}>{parseMarkdown(source).map(block)}</div>;
}

function block(b: MdBlock, key: number): ReactNode {
  switch (b.kind) {
    case "heading":
      return createElement(`h${b.level}`, { key }, runs(b.children));
    case "paragraph":
      return <p key={key}>{runs(b.children)}</p>;
    case "list": {
      const items = b.items.map((item, i) => <li key={i}>{runs(item)}</li>);
      return b.ordered ? (
        <ol key={key} start={b.start === 1 ? undefined : b.start}>
          {items}
        </ol>
      ) : (
        <ul key={key}>{items}</ul>
      );
    }
    case "quote":
      return (
        <blockquote key={key} className="an-md-quote">
          {runs(b.children)}
        </blockquote>
      );
    case "code":
      return (
        <pre key={key} className="an-md-code">
          <code>{b.text}</code>
        </pre>
      );
    case "rule":
      return <hr key={key} />;
  }
}

function runs(inline: Inline[]): ReactNode[] {
  return inline.map((r, i) => {
    switch (r.kind) {
      case "text":
        return r.text;
      case "code":
        return <code key={i}>{r.text}</code>;
      case "strong":
        return <strong key={i}>{runs(r.children)}</strong>;
      case "em":
        return <em key={i}>{runs(r.children)}</em>;
      case "link":
        return (
          <a key={i} href={r.href} target="_blank" rel="noopener noreferrer">
            {runs(r.children)}
          </a>
        );
    }
  });
}
