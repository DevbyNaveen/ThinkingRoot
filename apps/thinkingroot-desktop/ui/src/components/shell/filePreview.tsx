/**
 * Workspace file preview — Markdown rendering + Prism highlighting for code,
 * matching patterns used in ReadmeView + ChatView.
 */
import type { ComponentProps } from "react";
import ReactMarkdown from "react-markdown";
import rehypeSanitize from "rehype-sanitize";
import remarkGfm from "remark-gfm";
import { Prism as SyntaxHighlighter } from "react-syntax-highlighter";
import { vscDarkPlus } from "react-syntax-highlighter/dist/esm/styles/prism";

function extOf(path: string): string {
  const base = path.replace(/^.*\//, "");
  const i = base.lastIndexOf(".");
  if (i <= 0) return "";
  return base.slice(i + 1).toLowerCase();
}

/** Prism language id; use "markdown" only when embedding in md — handled separately */
function prismLanguageForPath(path: string): string {
  const ext = extOf(path);
  const map: Record<string, string> = {
    ts: "typescript",
    tsx: "tsx",
    jsx: "jsx",
    js: "javascript",
    mjs: "javascript",
    cjs: "javascript",
    json: "json",
    jsonc: "json",
    md: "markdown",
    mdx: "markdown",
    css: "css",
    scss: "scss",
    less: "less",
    html: "markup",
    htm: "markup",
    xml: "markup",
    svg: "markup",
    pom: "markup",
    gradle: "groovy",
    java: "java",
    kt: "kotlin",
    kts: "kotlin",
    rs: "rust",
    go: "go",
    py: "python",
    rb: "ruby",
    php: "php",
    cs: "csharp",
    fs: "fsharp",
    swift: "swift",
    mm: "clike",
    m: "clike",
    h: "clike",
    c: "clike",
    cc: "cpp",
    cpp: "cpp",
    cxx: "cpp",
    hpp: "cpp",
    sh: "bash",
    bash: "bash",
    zsh: "bash",
    fish: "bash",
    yml: "yaml",
    yaml: "yaml",
    toml: "toml",
    ini: "ini",
    sql: "sql",
    graphql: "graphql",
    gql: "graphql",
    vue: "markup",
    svelte: "markup",
    r: "r",
    dex: "clike",
  };
  return map[ext] ?? "plaintext";
}

export function isMarkdownPath(path: string): boolean {
  const e = extOf(path);
  return e === "md" || e === "markdown" || e === "mdx";
}

/** react-markdown code renderer — `any` matches ChatView / remark AST props */
function MdCode(props: any) {
  const { inline, className, children, ...rest } = props;
  const match = /language-(\w+)/.exec(className || "");
  if (!inline && match) {
    return (
      <div className="not-prose my-3 w-full overflow-x-auto">
        <SyntaxHighlighter
          language={match[1]}
          style={vscDarkPlus as ComponentProps<typeof SyntaxHighlighter>["style"]}
          PreTag="div"
          customStyle={{
            margin: 0,
            padding: "10px 12px",
            fontSize: "11px",
            lineHeight: 1.55,
            background: "#1e1e1e",
          }}
        >
          {String(children).replace(/\n$/, "")}
        </SyntaxHighlighter>
      </div>
    );
  }
  return (
    <code
      className="rounded bg-muted/80 px-1 py-0.5 font-mono text-[12px] text-foreground before:content-none after:content-none"
      {...rest}
    >
      {children}
    </code>
  );
}

export function FilePreviewContent({
  path,
  text,
}: {
  path: string;
  text: string;
}) {
  if (isMarkdownPath(path)) {
    return (
      <article className="prose prose-sm prose-invert max-w-none px-4 py-3 text-[13px] leading-relaxed prose-pre:bg-transparent prose-pre:p-0">
        <ReactMarkdown
          remarkPlugins={[remarkGfm]}
          rehypePlugins={[rehypeSanitize]}
          components={{ code: MdCode }}
        >
          {text}
        </ReactMarkdown>
      </article>
    );
  }

  const lang = prismLanguageForPath(path);

  if (lang === "plaintext" || lang === "markdown") {
    return (
      <pre className="m-0 whitespace-pre-wrap break-words bg-[#1e1e1e] p-3 font-mono text-[12px] leading-relaxed text-[#d4d4d4]">
        {text}
      </pre>
    );
  }

  /* Full-pane editor-style view: no card border, no chrome bar (same idea as VS Code body) */
  return (
    <SyntaxHighlighter
      language={lang}
      style={vscDarkPlus as ComponentProps<typeof SyntaxHighlighter>["style"]}
      PreTag="div"
      showLineNumbers
      wrapLongLines
      customStyle={{
        margin: 0,
        padding: "8px 12px 20px 0",
        fontSize: "12px",
        lineHeight: 1.55,
        background: "#1e1e1e",
        borderRadius: 0,
      }}
      lineNumberStyle={{
        minWidth: "2.5rem",
        paddingRight: "1rem",
        paddingLeft: "8px",
        color: "#858585",
        fontSize: "11px",
        userSelect: "none",
      }}
    >
      {text.endsWith("\n") ? text.slice(0, -1) : text}
    </SyntaxHighlighter>
  );
}
