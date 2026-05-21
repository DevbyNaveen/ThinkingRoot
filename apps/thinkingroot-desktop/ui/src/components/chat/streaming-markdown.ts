/**
 * Split assistant stream text into a markdown-safe prefix and a live tail.
 * The tail stays plain text so incomplete fences/tables do not re-layout.
 */

const MIN_TAIL_CHARS = 40;
const MAX_TAIL_CHARS = 280;

export interface StreamingMarkdownSplit {
  committed: string;
  tail: string;
}

export function splitStreamingMarkdown(source: string): StreamingMarkdownSplit {
  if (!source) {
    return { committed: "", tail: "" };
  }

  const fenceMatches = [...source.matchAll(/```/g)];
  if (fenceMatches.length % 2 === 1) {
    const openAt = fenceMatches[fenceMatches.length - 1]!.index ?? 0;
    return {
      committed: source.slice(0, openAt),
      tail: source.slice(openAt),
    };
  }

  let splitAt = source.length;

  const paragraphBreak = source.lastIndexOf("\n\n");
  if (paragraphBreak !== -1 && source.length - paragraphBreak > MIN_TAIL_CHARS) {
    splitAt = Math.min(splitAt, paragraphBreak + 2);
  }

  const lineBreak = source.lastIndexOf("\n");
  if (lineBreak !== -1 && source.length - lineBreak > MAX_TAIL_CHARS) {
    splitAt = Math.min(splitAt, lineBreak + 1);
  }

  if (splitAt <= 0 || splitAt >= source.length - MIN_TAIL_CHARS / 2) {
    return { committed: "", tail: source };
  }

  return {
    committed: source.slice(0, splitAt),
    tail: source.slice(splitAt),
  };
}
