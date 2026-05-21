/** Parsed YAML frontmatter header from `.thinkingroot/paper.md`. */
export interface PaperFrontmatterPreview {
  workspace?: string;
  witness_count?: number;
  source_count?: number;
}

/** Split `paper.md` at the YAML frontmatter fence for display. */
export function splitPaperFrontmatter(markdown: string | undefined): {
  frontmatter: PaperFrontmatterPreview | null;
  body: string;
} {
  if (!markdown) return { frontmatter: null, body: "" };
  if (!markdown.startsWith("---\n")) {
    return { frontmatter: null, body: markdown };
  }
  const rest = markdown.slice(4);
  const endIdx = rest.indexOf("\n---");
  if (endIdx < 0) return { frontmatter: null, body: markdown };
  const fmYaml = rest.slice(0, endIdx);
  const fm: PaperFrontmatterPreview = {};
  for (const line of fmYaml.split("\n")) {
    const m = line.match(/^([a-z_]+):\s*(.*)$/);
    if (!m) continue;
    const key = m[1] ?? "";
    const value = (m[2] ?? "").trim();
    if (key === "workspace") fm.workspace = value;
    else if (key === "witness_count") fm.witness_count = parseInt(value, 10);
    else if (key === "source_count") fm.source_count = parseInt(value, 10);
  }
  const bodyStart = endIdx + 4;
  const newline = rest.indexOf("\n", bodyStart);
  const body = newline >= 0 ? rest.slice(newline + 1).trimStart() : "";
  return { frontmatter: fm, body };
}
