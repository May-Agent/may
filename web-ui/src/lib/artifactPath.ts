// Pure path-manipulation helpers for the artifact panel. These used to
// live inline in ChatInterface as useCallback wrappers; they don't
// touch React state so they belong in a pure module.

const LOCALHOST_PATTERN =
  /^(localhost|127(?:\.\d{1,3}){3}|0\.0\.0\.0|\[::1\]|::1|host\.docker\.internal|host\.containers\.internal)(:\d+)?(\/.*)?$/i;

/**
 * Normalize a user-supplied string into an http(s) URL if it looks
 * like a preview target (a real URL or a localhost-ish authority).
 * Returns null for anything else.
 */
export function normalizePreviewUrl(value: string): string | null {
  const trimmed = value.trim();
  if (!trimmed) return null;

  try {
    const parsed = new URL(trimmed);
    if (parsed.protocol === 'http:' || parsed.protocol === 'https:') {
      return parsed.toString();
    }
    return null;
  } catch {
    if (!LOCALHOST_PATTERN.test(trimmed)) {
      return null;
    }
    try {
      return new URL(`http://${trimmed}`).toString();
    } catch {
      return null;
    }
  }
}

/** True when a string would be accepted by normalizePreviewUrl. */
export function isPreviewUrl(value: string): boolean {
  return normalizePreviewUrl(value) !== null;
}

/**
 * Normalize a raw artifact path — strip leading `./`, convert backslashes
 * to forward slashes, and collapse an `artifacts/` prefix because the
 * backend stores paths relative to the artifacts root.
 */
export function normalizeArtifactPath(value: string): string {
  const trimmed = value.trim();
  if (!trimmed) return '';

  const slashNormalized = trimmed.replace(/\\/g, '/');
  const withoutDotPrefix = slashNormalized.replace(/^\.\/+/, '');
  const lower = withoutDotPrefix.toLowerCase();
  if (lower === 'artifacts') return '';
  if (lower.startsWith('artifacts/')) {
    return withoutDotPrefix.slice('artifacts/'.length);
  }
  return withoutDotPrefix;
}
