import { FileText, Folder } from "lucide-react";

import { isFileLikeInlineCode } from "./inline-reference";

const PATH_LINE_RE =
  /^(?:\/?[\w.@~+\-[\]()]+(?:[/\\][\w.@~+\-[\]()]+)+|[\w.-]+\.[\w]{1,12})$/;

function splitUserBody(body: string): { paths: string[]; text: string } {
  const lines = body.split("\n");
  const paths: string[] = [];
  const textLines: string[] = [];

  for (const line of lines) {
    const t = line.trim();
    if (t && (PATH_LINE_RE.test(t) || isFileLikeInlineCode(t))) {
      paths.push(t);
    } else {
      textLines.push(line);
    }
  }

  const text = textLines.join("\n").trim();
  return { paths, text };
}

function PathIcon({ path }: { path: string }) {
  if (path.endsWith("/")) {
    return <Folder className="size-3.5 shrink-0 text-muted-foreground/70" aria-hidden />;
  }
  return <FileText className="size-3.5 shrink-0 text-muted-foreground/70" aria-hidden />;
}

export function UserMessageContent({ body }: { body: string }) {
  const { paths, text } = splitUserBody(body);

  return (
    <div className="space-y-2.5">
      {paths.length > 0 ? (
        <div className="flex flex-col gap-1 border-t border-border/20 pt-2">
          {paths.map((p) => (
            <div
              key={p}
              className="flex min-w-0 items-center gap-2 text-[13px] leading-snug text-foreground/85"
            >
              <PathIcon path={p} />
              <span className="min-w-0 truncate font-mono">{p}</span>
            </div>
          ))}
        </div>
      ) : null}
      {text ? (
        <p className="whitespace-pre-wrap break-words text-[15px] font-normal leading-[1.55] text-foreground/95">
          {text}
        </p>
      ) : null}
    </div>
  );
}
