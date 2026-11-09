import { Fragment } from 'react';

export interface MarkPart {
  text: string;
  hit: boolean;
}

/**
 * Split text around each occurrence of the query. A literal, case-insensitive
 * substring, which is exactly how the server's transcript search matches, so
 * what gets marked is what was found.
 */
export function markParts(text: string, query: string): MarkPart[] {
  const needle = query.trim().toLowerCase();
  const lower = text.toLowerCase();
  // A few characters change length when lowered, which would shift the slices.
  if (!needle || lower.length !== text.length) return [{ text, hit: false }];

  const parts: MarkPart[] = [];
  let from = 0;
  for (let at = lower.indexOf(needle); at !== -1; at = lower.indexOf(needle, from)) {
    if (at > from) parts.push({ text: text.slice(from, at), hit: false });
    parts.push({ text: text.slice(at, at + needle.length), hit: true });
    from = at + needle.length;
  }
  if (from < text.length) parts.push({ text: text.slice(from), hit: false });
  return parts;
}

export const hasMatch = (text: string, query: string): boolean => {
  const needle = query.trim().toLowerCase();
  return needle !== '' && text.toLowerCase().includes(needle);
};

/** Text with the query marked, for a reader looking for that word. */
export function Marked({ text, query }: { text: string; query: string }) {
  return (
    <>
      {markParts(text, query).map((part, i) =>
        part.hit ? <mark key={i}>{part.text}</mark> : <Fragment key={i}>{part.text}</Fragment>,
      )}
    </>
  );
}
