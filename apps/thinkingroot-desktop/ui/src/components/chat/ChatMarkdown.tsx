import {
  Children,
  isValidElement,
  useCallback,
  useMemo,
  type CSSProperties,
  type ReactElement,
  type ReactNode,
} from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import rehypeSanitize from "rehype-sanitize";
import { Prism as SyntaxHighlighter } from "react-syntax-highlighter";
import { vscDarkPlus } from "react-syntax-highlighter/dist/esm/styles/prism";
import { AlertTriangle, Copy, FileText, Folder, Info, Lightbulb } from "lucide-react";
import { writeText } from "@tauri-apps/plugin-clipboard-manager";

import { cn } from "@/lib/utils";
import { toast } from "@/store/toast";
import { transformCitations } from "@/components/playground/CitationChip";
import { InlineMarkdownCode } from "./InlineMarkdownCode";
import { isFileLikeInlineCode } from "./inline-reference";
import { MermaidBlock } from "./MermaidBlock";

type CalloutKind = "note" | "tip" | "important" | "warning" | "caution";

const CALLOUT_STYLES: Record<
  CalloutKind,
  { label: string; border: string; bg: string; icon: ReactNode }
> = {
  note: {
    label: "Note",
    border: "border-sky-500/35",
    bg: "bg-sky-500/[0.08]",
    icon: <Info className="size-4 shrink-0 text-sky-400" aria-hidden />,
  },
  tip: {
    label: "Tip",
    border: "border-emerald-500/35",
    bg: "bg-emerald-500/[0.08]",
    icon: <Lightbulb className="size-4 shrink-0 text-emerald-400" aria-hidden />,
  },
  important: {
    label: "Important",
    border: "border-violet-500/35",
    bg: "bg-violet-500/[0.08]",
    icon: <Info className="size-4 shrink-0 text-violet-400" aria-hidden />,
  },
  warning: {
    label: "Warning",
    border: "border-amber-500/35",
    bg: "bg-amber-500/[0.08]",
    icon: <AlertTriangle className="size-4 shrink-0 text-amber-400" aria-hidden />,
  },
  caution: {
    label: "Caution",
    border: "border-rose-500/35",
    bg: "bg-rose-500/[0.08]",
    icon: <AlertTriangle className="size-4 shrink-0 text-rose-400" aria-hidden />,
  },
};

function extractText(node: ReactNode): string {
  if (node == null || typeof node === "boolean") return "";
  if (typeof node === "string") return node;
  if (typeof node === "number") return String(node);
  if (Array.isArray(node)) return node.map(extractText).join("");
  if (isValidElement(node)) {
    return extractText((node as ReactElement<{ children?: ReactNode }>).props
      .children);
  }
  return "";
}

function maybeWrapCitations(children: ReactNode, enabled: boolean): ReactNode {
  return enabled ? transformCitations(children) : children;
}

function liIsSingleFileRef(children: ReactNode): boolean {
  const items = Children.toArray(children).filter(
    (c) => !(typeof c === "string" && !c.trim()),
  );
  if (items.length !== 1) return false;
  const node = items[0];
  if (isValidElement(node) && node.type === "code") {
    const props = node.props as { children?: ReactNode };
    return isFileLikeInlineCode(String(props.children ?? ""));
  }
  return false;
}

function ulIsFileRefList(children: ReactNode): boolean {
  const items = Children.toArray(children).filter(Boolean);
  if (items.length === 0) return false;
  return items.every(
    (item) =>
      isValidElement(item) &&
      item.type === "li" &&
      liIsSingleFileRef((item.props as { children?: ReactNode }).children),
  );
}

function fileRefLabel(children: ReactNode): string {
  const items = Children.toArray(children).filter(
    (c) => !(typeof c === "string" && !c.trim()),
  );
  if (items.length === 1 && isValidElement(items[0]) && items[0].type === "code") {
    return String((items[0].props as { children?: ReactNode }).children ?? "").trim();
  }
  return "";
}

function FileRefListItem({
  children,
  citations,
}: {
  children?: ReactNode;
  citations: boolean;
}) {
  const label = fileRefLabel(children);
  const isDir = label.endsWith("/");
  const Icon = isDir ? Folder : FileText;
  return (
    <li className="flex min-w-0 items-center gap-2.5 py-1 text-[13px] leading-snug text-foreground/88">
      <Icon className="size-3.5 shrink-0 text-muted-foreground/55" aria-hidden />
      <span className="min-w-0 flex-1 font-mono text-[0.92em]">
        {maybeWrapCitations(children, citations)}
      </span>
    </li>
  );
}

function FileRefList({ children }: { children?: ReactNode }) {
  return (
    <div className="file-ref-panel my-3 rounded-lg border border-border/25 bg-foreground/[0.03] px-3 py-2">
      <p className="mb-1.5 text-[10px] font-medium uppercase tracking-wider text-muted-foreground/55">
        In workspace
      </p>
      <ul className="list-none space-y-0 pl-0">{children}</ul>
    </div>
  );
}

function parseCallout(
  children: ReactNode,
): { kind: CalloutKind; body: ReactNode } | null {
  const items = Children.toArray(children);
  if (items.length === 0) return null;

  const firstText = extractText(items[0]).trim();
  const blockMatch = firstText.match(
    /^\[!(NOTE|TIP|IMPORTANT|WARNING|CAUTION)\]\s*$/i,
  );
  const blockLabel = blockMatch?.[1];
  if (blockLabel) {
    const kind = blockLabel.toLowerCase() as CalloutKind;
    const body = items.slice(1);
    return { kind, body: body.length > 0 ? body : null };
  }

  const inlineMatch = firstText.match(
    /^\[!(NOTE|TIP|IMPORTANT|WARNING|CAUTION)\]\s+([\s\S]+)$/i,
  );
  const inlineLabel = inlineMatch?.[1];
  const inlineBody = inlineMatch?.[2];
  if (inlineLabel && inlineBody) {
    const kind = inlineLabel.toLowerCase() as CalloutKind;
    return { kind, body: inlineBody };
  }

  return null;
}

function MarkdownCallout({
  kind,
  children,
}: {
  kind: CalloutKind;
  children: ReactNode;
}) {
  const style = CALLOUT_STYLES[kind];
  return (
    <aside
      className={cn(
        "my-5 flex gap-3 rounded-xl border px-4 py-3 text-[14px] leading-relaxed",
        style.border,
        style.bg,
      )}
    >
      {style.icon}
      <div className="min-w-0 flex-1">
        <p className="mb-1 text-[11px] font-semibold uppercase tracking-wide text-muted-foreground/80">
          {style.label}
        </p>
        <div className="text-foreground/90 [&>p:last-child]:mb-0 [&>p]:mb-2">
          {children}
        </div>
      </div>
    </aside>
  );
}

function CodeFence({ language, code }: { language: string; code: string }) {
  const copy = useCallback(async () => {
    try {
      await writeText(code);
      toast("Copied", { kind: "info" });
    } catch (e) {
      toast("Copy failed", {
        kind: "error",
        body: e instanceof Error ? e.message : String(e),
      });
    }
  }, [code]);

  const showLang = language !== "text";

  return (
    <div className="group/code relative my-5 overflow-hidden rounded-xl border border-border/55 bg-[hsl(0,0%,10%)]">
      {showLang ? (
        <span className="pointer-events-none absolute left-2.5 top-2 z-[1] font-mono text-[9px] font-medium uppercase tracking-wider text-muted-foreground/55">
          {language}
        </span>
      ) : null}
      <button
        type="button"
        onClick={() => void copy()}
        className="absolute right-2 top-2 z-[1] inline-flex items-center gap-1 rounded-md px-1.5 py-0.5 text-[10px] text-muted-foreground/70 opacity-0 transition-[opacity,color,background-color] hover:bg-muted/40 hover:text-foreground group-hover/code:opacity-100 focus-visible:opacity-100 focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-ring/50"
        aria-label="Copy code"
      >
        <Copy className="size-3" aria-hidden />
        Copy
      </button>
      <SyntaxHighlighter
        style={vscDarkPlus as Record<string, CSSProperties>}
        language={language === "text" ? undefined : language}
        PreTag="div"
        customStyle={{
          margin: 0,
          background: "transparent",
          padding: showLang ? "26px 52px 14px 14px" : "14px 52px 14px 16px",
          fontSize: "13px",
          lineHeight: "1.55",
        }}
      >
        {code}
      </SyntaxHighlighter>
    </div>
  )
}

function useMarkdownComponents(citations: boolean) {
  return useMemo(
    () => ({
      code({
        className,
        children,
        ...props
      }: {
        className?: string;
        children?: ReactNode;
      }) {
        const text = String(children ?? "").replace(/\n$/, "");
        const match = /language-(\w+)/.exec(className ?? "");
        const lang = match?.[1]?.toLowerCase();

        if (lang === "mermaid") {
          return <MermaidBlock code={text} />;
        }

        const isBlock = Boolean(match) || text.includes("\n");
        if (!isBlock) {
          return <InlineMarkdownCode text={text} {...props} />;
        }

        return <CodeFence language={lang ?? "text"} code={text} />;
      },
      p: ({ children }: { children?: ReactNode }) => (
        <p className="mb-3.5 last:mb-0 text-[15px] leading-[1.75] text-foreground/90">
          {maybeWrapCitations(children, citations)}
        </p>
      ),
      strong: ({ children }: { children?: ReactNode }) => (
        <strong className="font-semibold text-foreground">{children}</strong>
      ),
      ul: ({ children }: { children?: ReactNode }) => {
        const fileList = ulIsFileRefList(children);
        if (fileList) {
          return <FileRefList>{children}</FileRefList>;
        }
        return (
          <ul className="mb-3 list-disc space-y-1 pl-6 last:mb-0 marker:text-muted-foreground/60">
            {children}
          </ul>
        );
      },
      ol: ({ children }: { children?: ReactNode }) => (
        <ol className="mb-4 list-decimal space-y-1.5 pl-6 last:mb-0 marker:text-muted-foreground/70">
          {children}
        </ol>
      ),
      li: ({ children }: { children?: ReactNode }) => {
        if (liIsSingleFileRef(children)) {
          return <FileRefListItem citations={citations}>{children}</FileRefListItem>;
        }
        return (
          <li className="leading-[1.7] text-foreground/90">
            {maybeWrapCitations(children, citations)}
          </li>
        );
      },
      h1: ({ children }: { children?: ReactNode }) => (
        <h1 className="mb-4 mt-8 text-2xl font-semibold tracking-tight first:mt-0">
          {children}
        </h1>
      ),
      h2: ({ children }: { children?: ReactNode }) => (
        <h2 className="mb-2.5 mt-6 border-b border-border/30 pb-1.5 text-lg font-semibold tracking-tight text-foreground first:mt-0">
          {children}
        </h2>
      ),
      h3: ({ children }: { children?: ReactNode }) => (
        <h3 className="mb-2 mt-5 text-base font-semibold text-foreground first:mt-0">
          {children}
        </h3>
      ),
      h4: ({ children }: { children?: ReactNode }) => (
        <h4 className="mb-2 mt-5 text-base font-semibold first:mt-0">{children}</h4>
      ),
      a: ({ href, children }: { href?: string; children?: ReactNode }) => (
        <a
          href={href}
          target="_blank"
          rel="noopener noreferrer"
          className="font-medium text-sky-400/95 underline decoration-sky-400/35 underline-offset-[3px] hover:text-sky-300"
        >
          {children}
        </a>
      ),
      blockquote: ({ children }: { children?: ReactNode }) => {
        const callout = parseCallout(children);
        if (callout) {
          return (
            <MarkdownCallout kind={callout.kind}>
              {callout.body}
            </MarkdownCallout>
          );
        }
        return (
          <blockquote className="my-5 border-l-[3px] border-muted-foreground/35 py-0.5 pl-4 text-[15px] italic leading-relaxed text-muted-foreground">
            {children}
          </blockquote>
        );
      },
      hr: () => <hr className="my-8 border-border/50" />,
      table: ({ children }: { children?: ReactNode }) => (
        <div className="my-5 overflow-x-auto rounded-xl border border-border/55">
          <table className="w-full min-w-[28rem] border-collapse text-left text-[13px]">
            {children}
          </table>
        </div>
      ),
      thead: ({ children }: { children?: ReactNode }) => (
        <thead className="bg-muted/35 text-foreground">{children}</thead>
      ),
      tbody: ({ children }: { children?: ReactNode }) => (
        <tbody className="divide-y divide-border/40">{children}</tbody>
      ),
      tr: ({ children }: { children?: ReactNode }) => <tr>{children}</tr>,
      th: ({ children }: { children?: ReactNode }) => (
        <th className="px-3 py-2.5 font-semibold text-foreground/95">
          {maybeWrapCitations(children, citations)}
        </th>
      ),
      td: ({ children }: { children?: ReactNode }) => (
        <td className="px-3 py-2.5 text-foreground/88">
          {maybeWrapCitations(children, citations)}
        </td>
      ),
      pre: ({ children }: { children?: ReactNode }) => <>{children}</>,
    }),
    [citations],
  );
}

export function ChatMarkdown({
  children,
  className,
  citations = true,
}: {
  children: string;
  className?: string;
  /** When false, skip {@link transformCitations} (e.g. static docs). */
  citations?: boolean;
}) {
  const components = useMarkdownComponents(citations);

  return (
    <div
      className={cn(
        "chat-markdown text-[15px] leading-relaxed text-foreground",
        className,
      )}
    >
      <ReactMarkdown
        remarkPlugins={[remarkGfm]}
        rehypePlugins={[rehypeSanitize]}
        components={components}
      >
        {children}
      </ReactMarkdown>
    </div>
  );
}
