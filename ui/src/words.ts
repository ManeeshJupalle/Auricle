// How the interface says things. The API's ids stay in the API.

const PROVIDER_NAMES: Record<string, string> = {
  'whisper-local': 'Whisper',
  deepgram: 'Deepgram',
  'groq-whisper': 'Groq Whisper',
  groq: 'Groq',
  'openai-compat': 'OpenAI-compatible',
  ollama: 'Ollama',
};

/** A provider as a person would name it. An id we don't know is shown as it is. */
export const providerName = (id: string): string => PROVIDER_NAMES[id] ?? id;

/** A summary template's file name as a label: action-items reads "Action items". */
export function templateName(name: string): string {
  const words = name.replace(/[-_]/g, ' ');
  return words.charAt(0).toUpperCase() + words.slice(1);
}

/** Which side of the line a provider's data ends up on, when we know the provider. */
const PROVIDER_PLACES: Record<string, 'local' | 'cloud'> = {
  'whisper-local': 'local',
  ollama: 'local',
  deepgram: 'cloud',
  'groq-whisper': 'cloud',
  groq: 'cloud',
  'openai-compat': 'cloud',
};

export const providerPlace = (id: string): 'local' | 'cloud' | undefined => PROVIDER_PLACES[id];

/** A provider and where it runs, for anything recording who handled a meeting. */
export function providerWhere(id: string): string {
  const place = providerPlace(id);
  const where = place === 'local' ? ', on this machine' : place === 'cloud' ? ', in the cloud' : '';
  return `${providerName(id)}${where}`;
}

/** Where a model download stands. Once every byte is in, the engine is verifying it. */
export function downloadLine(d: { doneMb: number; sizeMb: number }): string {
  return d.doneMb >= d.sizeMb
    ? 'Checking the speech model'
    : // No-break spaces: the figures read as one, and must not split across lines.
      `Downloading the speech model, ${Math.floor(d.doneMb)}\u00a0of\u00a0${d.sizeMb}\u00a0MB`;
}

/**
 * A summary without its opening heading, when that only repeats the meeting's
 * title or the template's name, both already on the page. A heading that says
 * something new stays.
 */
export function withoutRepeatedTitle(content: string, ...known: string[]): string {
  const opening = /^\s*#{1,2}\s+(.+?)\s*(?:\r?\n|$)/.exec(content);
  if (!opening) return content;
  const heading = opening[1].toLowerCase();
  const repeats = known.some((name) => {
    const lower = name.toLowerCase();
    return lower.includes(heading) || heading.includes(lower);
  });
  return repeats ? content.slice(opening[0].length) : content;
}

/** Calendar days since a moment, so 11 pm last night is yesterday even at 1 am. */
export function daysAgo(unixSecs: number, now = new Date()): number {
  const startOfDay = (d: Date) => new Date(d.getFullYear(), d.getMonth(), d.getDate()).getTime();
  return Math.round((startOfDay(now) - startOfDay(new Date(unixSecs * 1000))) / 86_400_000);
}

/** When a meeting happened, in the same days the sidebar groups by. */
export function agoWord(unixSecs: number, now = new Date()): string {
  const days = daysAgo(unixSecs, now);
  if (days <= 0) return 'today';
  if (days === 1) return 'yesterday';
  return `${days} days ago`;
}
