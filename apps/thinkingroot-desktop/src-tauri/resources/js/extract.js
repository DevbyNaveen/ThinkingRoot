// extract.js — orchestrator for in-app browser "Save to ThinkingRoot".
//
// Injected via webview.eval() into the captive browser webview after
// Readability.js and Turndown.js have been concatenated ahead of it.
// Runs Mozilla's Readability over a clone of the document, hands the
// resulting article HTML to Turndown to convert into Markdown, then
// sends the payload back to Rust via Tauri's IPC bridge.
//
// The bridge is `window.__TAURI_INTERNALS__.invoke()`, which Tauri 2
// auto-injects on every webview built via WebviewBuilder. It uses the
// platform's native script-message-handler (WKScriptMessageHandler on
// macOS, equivalent on Win/Linux) and is NOT subject to the captive
// page's CSP — that's why we can extract from sites like google.com
// whose `connect-src` would block a regular fetch().
//
// The Rust side has stamped __TR_EXTRACT_REQ_ID with the request id
// (string) just before this file is eval'd so the callback can be
// routed to the right oneshot channel.
(async function () {
  const requestId = window.__TR_EXTRACT_REQ_ID;
  const invoker =
    window.__TAURI_INTERNALS__ && window.__TAURI_INTERNALS__.invoke;

  function send(payload) {
    if (typeof invoker === "function") {
      return invoker("browser_extract_callback", { requestId, payload });
    }
    // No bridge — log and bail. The Rust-side timeout will surface
    // the failure to the user.
    console.error(
      "[ThinkingRoot] __TAURI_INTERNALS__.invoke not available; cannot deliver extraction",
    );
    return Promise.resolve();
  }

  try {
    if (typeof Readability !== "function") {
      throw new Error("Readability.js missing");
    }
    if (typeof TurndownService !== "function") {
      throw new Error("Turndown.js missing");
    }

    // Readability mutates the cloned tree, so we always clone first
    // — otherwise re-extraction on the same page yields empty results.
    const docClone = document.cloneNode(true);
    const reader = new Readability(docClone, {
      charThreshold: 100,
      keepClasses: false,
    });
    const article = reader.parse();

    if (!article || !article.content) {
      await send({
        error:
          "Readability could not extract structured article content from this page",
        title: document.title || "Untitled",
        url: window.location.href,
      });
      return;
    }

    const turndown = new TurndownService({
      headingStyle: "atx",
      codeBlockStyle: "fenced",
      bulletListMarker: "-",
      emDelimiter: "_",
      hr: "---",
    });
    turndown.addRule("strikethrough", {
      filter: ["del", "s", "strike"],
      replacement: (content) => "~~" + content + "~~",
    });
    // Drop noisy elements Readability sometimes keeps.
    turndown.remove(["script", "style", "iframe", "form", "input", "button"]);

    let markdown = "";
    try {
      markdown = turndown.turndown(article.content);
    } catch (turndownErr) {
      // Fall back to article.textContent so we at least save SOMETHING
      // rather than dropping the whole page on a single turndown bug.
      markdown = String(article.textContent || "").trim();
      if (!markdown) {
        await send({
          error: "Turndown failed and no text fallback available: " + String(turndownErr),
          title: article.title || document.title || "Untitled",
          url: window.location.href,
        });
        return;
      }
    }

    await send({
      title: article.title || document.title || "Untitled",
      url: window.location.href,
      markdown,
      byline: article.byline || null,
      site_name: article.siteName || null,
      excerpt: article.excerpt || null,
      length: article.length || markdown.length,
    });
  } catch (e) {
    try {
      await send({
        error: String((e && e.message) || e),
        title: document.title || "Untitled",
        url: window.location.href,
      });
    } catch (_inner) {
      // give up — the Rust-side timeout will catch this.
    }
  }
})();
