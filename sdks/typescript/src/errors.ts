/**
 * Typed errors thrown by the ThinkingRoot SDK.
 *
 * - `ApiError` wraps the structured `{ ok: false, error: { code, message } }`
 *   envelope returned by every REST endpoint.
 * - `ConnectionError` wraps lower-level transport failures (network,
 *   timeout, missing daemon) so callers can branch on a single error
 *   type rather than `instanceof TypeError | FetchError | ...`.
 * - `CortexError` is raised by the cortex.lock parser when the on-disk
 *   schema is corrupt or future-versioned.
 */

export class ApiError extends Error {
  override readonly name = "ApiError";
  readonly statusCode: number;
  readonly code: string;

  constructor(statusCode: number, code: string, message: string) {
    super(`[${statusCode}] ${code}: ${message}`);
    this.statusCode = statusCode;
    this.code = code;
  }
}

export class ConnectionError extends Error {
  override readonly name = "ConnectionError";
  override readonly cause?: unknown;

  constructor(message: string, cause?: unknown) {
    super(message);
    this.cause = cause;
  }
}

export class CortexError extends Error {
  override readonly name: string = "CortexError";
  constructor(message: string) {
    super(message);
  }
}

export class IncompatibleLockSchema extends CortexError {
  override readonly name: string = "IncompatibleLockSchema";
}
