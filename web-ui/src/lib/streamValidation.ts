// Helpers that validate the pre-conditions for starting a chat stream.
// These are pure predicates / validation functions — no React state.

export interface ModelValidation {
  ok: boolean;
  error?: string;
}

/**
 * Check whether a given model id is configured for the current profile.
 *
 * `configuredModelIds` comes from the UI config; an empty set means
 * "we haven't loaded the model list yet" and we pass the check to
 * avoid blocking bootstrap.
 */
export function validateModelConfigured(
  modelId: string | undefined,
  configuredModelIds: ReadonlySet<string>,
  missingModelMessage: string,
  unavailableModelMessage: string,
): ModelValidation {
  const trimmed = modelId?.trim() ?? '';
  if (!trimmed) {
    return { ok: false, error: missingModelMessage };
  }
  if (configuredModelIds.size > 0 && !configuredModelIds.has(trimmed)) {
    return { ok: false, error: unavailableModelMessage };
  }
  return { ok: true };
}

/**
 * True when the runtime snapshot indicates the session is still
 * actively doing work (running, waiting, or has anything queued).
 * Used to decide whether a new message should be enqueued vs. sent
 * as the primary request.
 */
export function isRuntimeTargetBusy(runtime: {
  state: string;
  pendingImmediate?: unknown;
  queuedMessages?: readonly unknown[];
}): boolean {
  return (
    runtime.state !== 'idle' ||
    runtime.pendingImmediate != null ||
    (runtime.queuedMessages?.length ?? 0) > 0
  );
}
