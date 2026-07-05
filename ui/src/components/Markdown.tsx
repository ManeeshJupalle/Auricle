import { Fragment, type ReactNode } from 'react';

// Minimal markdown for LLM summary output: headers, bullet/numbered lists,
// checklists, bold/italic/inline-code. Parses to data (unit-testable) and
// renders as React elements — no innerHTML, so LLM output can't inject
// markup. Unrecognized syntax falls through as plain text.

export type Block =
  | { kind: 'heading'; level: number; text: string }
  | { kind: 'bullet'; items: string[] }
  | { kind: 'ordered'; items: string[] }
  | { kind: 'check'; items: { checked: boolean; text: string }[] }
  | { kind: 'para'; text: string };

export function parseBlocks(src: string): Block[] {
  const blocks: Block[] = [];
  const lines = src.split(/\r?\n/);
  let i = 0;
  while (i < lines.length) {
    const line = lines[i];
    const trimmed = line.trim();
    if (trimmed === '') {
      i += 1;
      continue;
    }
    const heading = /^(#{1,4})\s+(.*)$/.exec(trimmed);
    if (heading) {
      blocks.push({ kind: 'heading', level: heading[1].length, text: heading[2] });
      i += 1;
      continue;
    }
    const check = /^[-*]\s+\[( |x|X)\]\s+(.*)$/.exec(trimmed);
    if (check) {
      const items: { checked: boolean; text: string }[] = [];
      while (i < lines.length) {
        const m = /^[-*]\s+\[( |x|X)\]\s+(.*)$/.exec(lines[i].trim());
        if (!m) break;
        items.push({ checked: m[1].toLowerCase() === 'x', text: m[2] });
        i += 1;
      }
      blocks.push({ kind: 'check', items });
      continue;
    }
    if (/^[-*]\s+/.test(trimmed)) {
      const items: string[] = [];
      while (i < lines.length && /^[-*]\s+/.test(lines[i].trim())) {
        items.push(lines[i].trim().replace(/^[-*]\s+/, ''));
        i += 1;
      }
      blocks.push({ kind: 'bullet', items });
      continue;
    }
    if (/^\d+[.)]\s+/.test(trimmed)) {
      const items: string[] = [];
      while (i < lines.length && /^\d+[.)]\s+/.test(lines[i].trim())) {
        items.push(lines[i].trim().replace(/^\d+[.)]\s+/, ''));
        i += 1;
      }
      blocks.push({ kind: 'ordered', items });
      continue;
    }
    // Paragraph: consecutive non-empty, non-structural lines.
    const para: string[] = [];
    while (i < lines.length) {
      const t = lines[i].trim();
      if (
        t === '' ||
        /^#{1,4}\s+/.test(t) ||
        /^[-*]\s+/.test(t) ||
        /^\d+[.)]\s+/.test(t)
      ) {
        break;
      }
      para.push(t);
      i += 1;
    }
    blocks.push({ kind: 'para', text: para.join(' ') });
  }
  return blocks;
}

export type Span =
  | { kind: 'text'; text: string }
  | { kind: 'bold'; text: string }
  | { kind: 'italic'; text: string }
  | { kind: 'code'; text: string };

export function parseInline(text: string): Span[] {
  const spans: Span[] = [];
  // Longest-marker-first so ** is not eaten as two *.
  const re = /(\*\*([^*]+)\*\*|`([^`]+)`|\*([^*]+)\*)/g;
  let last = 0;
  for (let m = re.exec(text); m !== null; m = re.exec(text)) {
    if (m.index > last) spans.push({ kind: 'text', text: text.slice(last, m.index) });
    if (m[2] !== undefined) spans.push({ kind: 'bold', text: m[2] });
    else if (m[3] !== undefined) spans.push({ kind: 'code', text: m[3] });
    else if (m[4] !== undefined) spans.push({ kind: 'italic', text: m[4] });
    last = m.index + m[0].length;
  }
  if (last < text.length) spans.push({ kind: 'text', text: text.slice(last) });
  return spans;
}

function Inline({ text }: { text: string }) {
  return (
    <>
      {parseInline(text).map((s, i) => {
        switch (s.kind) {
          case 'bold':
            return <strong key={i}>{s.text}</strong>;
          case 'italic':
            return <em key={i}>{s.text}</em>;
          case 'code':
            return <code key={i}>{s.text}</code>;
          default:
            return <Fragment key={i}>{s.text}</Fragment>;
        }
      })}
    </>
  );
}

export function Markdown({ source }: { source: string }) {
  const blocks = parseBlocks(source);
  return (
    <div className="md">
      {blocks.map((b, i): ReactNode => {
        switch (b.kind) {
          case 'heading':
            return (
              <div key={i} className={`md-h md-h${b.level}`}>
                <Inline text={b.text} />
              </div>
            );
          case 'bullet':
            return (
              <ul key={i}>
                {b.items.map((item, j) => (
                  <li key={j}>
                    <Inline text={item} />
                  </li>
                ))}
              </ul>
            );
          case 'ordered':
            return (
              <ol key={i}>
                {b.items.map((item, j) => (
                  <li key={j}>
                    <Inline text={item} />
                  </li>
                ))}
              </ol>
            );
          case 'check':
            return (
              <ul key={i} className="md-checks">
                {b.items.map((item, j) => (
                  <li key={j}>
                    <input type="checkbox" checked={item.checked} readOnly />{' '}
                    <Inline text={item.text} />
                  </li>
                ))}
              </ul>
            );
          default:
            return (
              <p key={i}>
                <Inline text={b.text} />
              </p>
            );
        }
      })}
    </div>
  );
}
