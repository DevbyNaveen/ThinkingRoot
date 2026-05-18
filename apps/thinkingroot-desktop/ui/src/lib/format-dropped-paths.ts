const IMAGE_EXTENSIONS = new Set([
  "png",
  "jpg",
  "jpeg",
  "webp",
  "gif",
  "svg",
  "heic",
  "bmp",
  "tif",
  "tiff",
]);

/** Turn OS file paths into composer snippets (image markdown or plain paths). */
export function formatDroppedPathsForComposer(paths: string[]): string {
  return paths
    .map((p) => {
      const ext = p.split(".").pop()?.toLowerCase() ?? "";
      if (IMAGE_EXTENSIONS.has(ext)) return `![](${p})\n`;
      return `${p}\n`;
    })
    .join("");
}

export const COMPOSER_FILE_DROP_EVENT = "thinkingroot:composer-file-drop";

export function emitComposerFileDrop(paths: string[]) {
  window.dispatchEvent(
    new CustomEvent<string[]>(COMPOSER_FILE_DROP_EVENT, { detail: paths }),
  );
}
