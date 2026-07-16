import { describe, expect, it } from 'vitest';
import { parseBlocks, parseInline } from './Markdown';

describe('markdown blocks', () => {
  it('parses the shapes LLM summaries actually produce', () => {
    const src = [
      '## Done',
      'Reported the latency numbers.',
      '',
      '* first bullet',
      '- second bullet',
      '1. step one',
      '2) step two',
      '- [ ] open task',
      '- [x] closed task',
    ].join('\n');
    const blocks = parseBlocks(src);
    expect(blocks).toEqual([
      { kind: 'heading', level: 2, text: 'Done' },
      { kind: 'para', text: 'Reported the latency numbers.' },
      { kind: 'bullet', items: ['first bullet', 'second bullet'] },
      { kind: 'ordered', items: ['step one', 'step two'] },
      {
        kind: 'check',
        items: [
          { checked: false, text: 'open task' },
          { checked: true, text: 'closed task' },
        ],
      },
    ]);
  });

  it('joins consecutive plain lines into one paragraph', () => {
    const blocks = parseBlocks('line one\nline two\n\nline three');
    expect(blocks).toEqual([
      { kind: 'para', text: 'line one line two' },
      { kind: 'para', text: 'line three' },
    ]);
  });

  it('handles the real Groq minutes shape', () => {
    const src =
      'The purpose of the meeting was X. \n* Topic A was discussed.\n**Decisions**: none.';
    const blocks = parseBlocks(src);
    expect(blocks[0].kind).toBe('para');
    expect(blocks[1]).toEqual({ kind: 'bullet', items: ['Topic A was discussed.'] });
    expect(blocks[2].kind).toBe('para');
  });
});

describe('markdown inline', () => {
  it('parses bold, italic, and code without eating adjacent text', () => {
    expect(parseInline('a **bold** and *ital* and `code` end')).toEqual([
      { kind: 'text', text: 'a ' },
      { kind: 'bold', text: 'bold' },
      { kind: 'text', text: ' and ' },
      { kind: 'italic', text: 'ital' },
      { kind: 'text', text: ' and ' },
      { kind: 'code', text: 'code' },
      { kind: 'text', text: ' end' },
    ]);
  });

  it('never emits markup for HTML in the source (XSS safety is structural)', () => {
    const spans = parseInline('<script>alert(1)</script> **<b>x</b>**');
    // Everything comes back as data; the renderer emits React text nodes.
    expect(spans).toEqual([
      { kind: 'text', text: '<script>alert(1)</script> ' },
      { kind: 'bold', text: '<b>x</b>' },
    ]);
  });

  it('plain text passes through untouched', () => {
    expect(parseInline('no markup here')).toEqual([{ kind: 'text', text: 'no markup here' }]);
  });
});
