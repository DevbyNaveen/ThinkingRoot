/** Classify markdown `code` spans — file/path refs vs real inline code. */
export function isFileLikeInlineCode(text: string): boolean {
  const t = text.trim();
  if (!t || t.length > 160 || t.includes("\n")) return false;

  if (/[{}();]|=>|::|->|\|\||&&/.test(t)) return false;

  if (t.endsWith("/") || /[/\\]/.test(t)) return true;

  if (/\.[a-z0-9]{1,12}$/i.test(t) && /^[\w@#.~+\-[\]()]+$/.test(t)) return true;

  if (
    /^[\w][\w.-]{0,56}$/.test(t) &&
    !/\s/.test(t) &&
    (t.includes(".") || /^[A-Z][A-Z0-9_.-]*$/.test(t))
  ) {
    return true;
  }

  return false;
}
