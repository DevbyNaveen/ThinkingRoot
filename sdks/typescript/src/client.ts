/**
 * Low-level fetch client for the ThinkingRoot REST API.
 *
 * The high-level `Brain` facade composes this client; advanced users
 * who want raw API access can instantiate it directly.
 */

import { ApiError, ConnectionError } from "./errors.js";
import type { ApiEnvelope } from "./types.js";

export interface ClientOptions {
  baseUrl?: string;
  apiKey?: string | null;
  /** Request timeout in milliseconds.  Default: 120 000. */
  timeoutMs?: number;
  /** Optional `fetch` implementation override (testing seam). */
  fetch?: typeof fetch;
}

const DEFAULT_BASE_URL = "http://127.0.0.1:31760";
const DEFAULT_TIMEOUT_MS = 120_000;

/**
 * Fetch-based client for the ThinkingRoot REST API.  Methods are kept
 * thin — payload shaping happens here, schema typing happens in the
 * Brain facade.
 */
export class Client {
  readonly baseUrl: string;
  readonly apiKey: string | null;
  private readonly _fetch: typeof fetch;
  private readonly _timeoutMs: number;
  private readonly _apiPrefix = "/api/v1";

  constructor(opts: ClientOptions = {}) {
    this.baseUrl = (opts.baseUrl ?? DEFAULT_BASE_URL).replace(/\/$/, "");
    this.apiKey = opts.apiKey ?? null;
    this._fetch = opts.fetch ?? fetch;
    this._timeoutMs = opts.timeoutMs ?? DEFAULT_TIMEOUT_MS;
  }

  /** GET `<base>/api/v1<path>`. */
  async get<T>(path: string, headers: Record<string, string> = {}): Promise<T> {
    return this._request<T>("GET", path, undefined, headers);
  }

  /** POST `<base>/api/v1<path>` with optional JSON body. */
  async post<T>(
    path: string,
    body?: unknown,
    headers: Record<string, string> = {},
  ): Promise<T> {
    return this._request<T>("POST", path, body, headers);
  }

  /** DELETE `<base>/api/v1<path>`. */
  async del<T>(
    path: string,
    headers: Record<string, string> = {},
  ): Promise<T> {
    return this._request<T>("DELETE", path, undefined, headers);
  }

  private async _request<T>(
    method: string,
    path: string,
    body: unknown,
    extraHeaders: Record<string, string>,
  ): Promise<T> {
    const url = `${this.baseUrl}${this._apiPrefix}${path}`;
    const headers: Record<string, string> = {
      Accept: "application/json",
      ...extraHeaders,
    };
    if (this.apiKey) {
      headers["Authorization"] = `Bearer ${this.apiKey}`;
    }
    let payload: string | undefined;
    if (body !== undefined) {
      headers["Content-Type"] = "application/json";
      payload = JSON.stringify(body);
    }

    const ctrl = new AbortController();
    const timer = setTimeout(() => ctrl.abort(), this._timeoutMs);

    let resp: Response;
    try {
      resp = await this._fetch(url, {
        method,
        headers,
        body: payload,
        signal: ctrl.signal,
      });
    } catch (err) {
      if ((err as { name?: string }).name === "AbortError") {
        throw new ConnectionError(
          `${method} ${url} timed out after ${this._timeoutMs}ms`,
          err,
        );
      }
      throw new ConnectionError(`${method} ${url}: ${(err as Error).message}`, err);
    } finally {
      clearTimeout(timer);
    }

    let parsed: ApiEnvelope<T>;
    try {
      parsed = (await resp.json()) as ApiEnvelope<T>;
    } catch (err) {
      throw new ConnectionError(
        `${method} ${url}: response was not JSON (HTTP ${resp.status})`,
        err,
      );
    }

    if (!parsed.ok) {
      const errBody = parsed.error ?? { code: "UNKNOWN", message: "Unknown error" };
      throw new ApiError(resp.status, errBody.code, errBody.message);
    }
    // The envelope can carry `data: null` for void-shape endpoints.
    return (parsed.data ?? (undefined as unknown)) as T;
  }
}
