// Tool output parsing — extracted from MessageItem.tsx
// Pure functions, zero React dependencies.

export interface ParsedCommandImage {
  src: string;
  mediaType: string;
  source: 'inline' | 'stored';
  storagePath?: string;
}

export interface ParsedCommandOutput {
  text?: string;
  stdoutText?: string;
  stderrText?: string;
  exitCode?: number;
  images: ParsedCommandImage[];
  storagePaths: string[];
}

export function detectCodeLanguage(text: string): 'shell' | 'json' | 'text' {
  const trimmed = text.trim();
  if (!trimmed) return 'text';
  if (trimmed.startsWith('{') || trimmed.startsWith('[')) {
    try {
      JSON.parse(trimmed);
      return 'json';
    } catch {
      // fall through
    }
  }
  if (/(\$\s+|bash|zsh|sh\s|pwsh|powershell)/i.test(trimmed)) return 'shell';
  return 'text';
}

export function formatCodeText(text: string, language: 'shell' | 'json' | 'text'): string {
  if (language !== 'json') return text;
  try {
    return JSON.stringify(JSON.parse(text), null, 2);
  } catch {
    return text;
  }
}

export function parseDataUrlImage(value: string): ParsedCommandImage | null {
  const trimmed = value.trim();
  if (!trimmed.startsWith('data:image/')) {
    return null;
  }
  const match = /^data:(image\/[a-z0-9.+-]+);base64,([a-z0-9+/=\s]+)$/i.exec(trimmed);
  if (!match) {
    return null;
  }
  const mediaType = match[1].toLowerCase();
  const encoded = match[2].replace(/\s+/g, '');
  if (!encoded) {
    return null;
  }
  return {
    src: `data:${mediaType};base64,${encoded}`,
    mediaType,
    source: 'inline',
  };
}

export function isLikelyBinaryTextPayload(text: string): boolean {
  if (!text) {
    return false;
  }
  const sample = text.slice(0, 4096);
  const lowercaseSample = sample.toLowerCase();
  if (
    sample.length >= 64 &&
    (
      lowercaseSample.includes('jfif') ||
      lowercaseSample.includes('exif') ||
      lowercaseSample.includes('ihdr') ||
      lowercaseSample.includes('png') ||
      lowercaseSample.includes('webp') ||
      lowercaseSample.includes('riff')
    )
  ) {
    return true;
  }
  let suspiciousUnits = 0;
  let symbolicUnits = 0;
  let languageLikeUnits = 0;
  for (const char of sample) {
    const code = char.codePointAt(0) ?? 0;
    if (code === 0xfffd) {
      suspiciousUnits += 2;
      continue;
    }
    const isAsciiControl = code < 0x20 && code !== 0x09 && code !== 0x0a && code !== 0x0d;
    if (isAsciiControl || code === 0x7f || code === 0x00) {
      suspiciousUnits += 1;
    }
    if (/[\[\]{}<>\\|`~^]/.test(char)) {
      symbolicUnits += 1;
    }
    if (
      (code >= 0x30 && code <= 0x39) ||
      (code >= 0x41 && code <= 0x5a) ||
      (code >= 0x61 && code <= 0x7a) ||
      (code >= 0x4e00 && code <= 0x9fff)
    ) {
      languageLikeUnits += 1;
    }
  }
  if (suspiciousUnits > 0 && suspiciousUnits / Math.max(1, sample.length) > 0.02) {
    return true;
  }
  if (sample.length >= 320) {
    const symbolicRatio = symbolicUnits / sample.length;
    const languageLikeRatio = languageLikeUnits / sample.length;
    if (symbolicRatio > 0.16 && languageLikeRatio < 0.52) {
      return true;
    }
  }
  return false;
}

function parseJsonLike(value: string): unknown {
  const trimmed = value.trim();
  if (!trimmed) {
    return value;
  }
  const looksLikeJson =
    (trimmed.startsWith('{') && trimmed.endsWith('}')) ||
    (trimmed.startsWith('[') && trimmed.endsWith(']'));
  if (!looksLikeJson) {
    return value;
  }
  try {
    return JSON.parse(trimmed);
  } catch {
    return value;
  }
}

function unwrapStructuredOutputNode(value: unknown): unknown {
  if (!value || typeof value !== 'object' || Array.isArray(value)) {
    return value;
  }
  const node = value as Record<string, unknown>;
  const kind =
    typeof node.kind === 'string' ? node.kind.trim().toLowerCase() : '';
  if (kind === 'json' && node.value !== undefined) {
    return node.value;
  }
  if ((kind === 'text' || kind === 'error') && node.value !== undefined) {
    return node.value;
  }
  return value;
}

function parseOutputEntry(
  entry: unknown,
  parsed: ParsedCommandOutput,
  textChunks: string[],
  sourcePath?: string,
) {
  const unwrappedEntry = unwrapStructuredOutputNode(entry);
  if (unwrappedEntry !== entry) {
    parseOutputEntry(unwrappedEntry, parsed, textChunks, sourcePath);
    return;
  }
  if (entry === null || entry === undefined) {
    return;
  }
  if (typeof entry === 'string') {
    const inlineImage = parseDataUrlImage(entry);
    if (inlineImage) {
      parsed.images.push(inlineImage);
      return;
    }
    if (isLikelyBinaryTextPayload(entry)) {
      return;
    }
    if (entry.trim().length > 0) {
      textChunks.push(entry);
    }
    return;
  }
  if (Array.isArray(entry)) {
    for (const item of entry) {
      parseOutputEntry(item, parsed, textChunks, sourcePath);
    }
    return;
  }
  if (typeof entry !== 'object') {
    return;
  }
  const node = entry as Record<string, unknown>;
  const pathCandidates = [
    typeof node.url === 'string' ? node.url.trim() : '',
    typeof node.path === 'string' ? node.path.trim() : '',
    typeof node.storage_path === 'string' ? node.storage_path.trim() : '',
    typeof node.storagePath === 'string' ? node.storagePath.trim() : '',
    typeof node.output_path === 'string' ? node.output_path.trim() : '',
  ].filter((value) => value.length > 0);
  const effectiveSourcePath = pathCandidates[0] || sourcePath;
  if (pathCandidates.length > 0) {
    parsed.storagePaths.push(...pathCandidates);
  }
  const content = node.content;
  const payload =
    content && typeof content === 'object' && !Array.isArray(content)
      ? (unwrapStructuredOutputNode(content) as Record<string, unknown>)
      : node;
  const kindSource =
    typeof payload.type === 'string'
      ? payload.type
      : typeof payload.kind === 'string'
        ? payload.kind
        : '';
  const kind = kindSource.trim().toLowerCase();
  const textCandidate =
    typeof payload.text === 'string'
      ? payload.text
      : typeof payload.value === 'string'
        ? payload.value
        : typeof node.text === 'string'
          ? node.text
          : typeof node.value === 'string'
            ? node.value
            : '';
  if (kind === 'text' || (kind.length === 0 && textCandidate.trim().length > 0)) {
    const text = textCandidate;
    if (text.trim().length > 0) {
      if (isLikelyBinaryTextPayload(text)) {
        return;
      }
      textChunks.push(text);
    }
    return;
  }
  if (kind === 'image') {
    const encoded =
      typeof payload.data === 'string'
        ? payload.data.trim()
        : typeof payload.value === 'string'
          ? payload.value.trim()
          : '';
    if (!encoded) {
      return;
    }
    const mediaType =
      typeof payload.media_type === 'string' && payload.media_type.trim().length > 0
        ? payload.media_type.trim()
        : 'image/png';
    parsed.images.push({
      src: `data:${mediaType};base64,${encoded}`,
      mediaType,
      source: 'inline',
      storagePath: effectiveSourcePath || undefined,
    });
    return;
  }

  if (kind === 'image_url') {
    const imageUrl =
      typeof payload.image_url === 'string'
        ? payload.image_url.trim()
        : payload.image_url && typeof payload.image_url === 'object'
          ? typeof (payload.image_url as Record<string, unknown>).url === 'string'
            ? ((payload.image_url as Record<string, unknown>).url as string).trim()
            : ''
          : typeof payload.url === 'string'
            ? payload.url.trim()
            : '';
    if (!imageUrl) {
      return;
    }
    const inlineImage = parseDataUrlImage(imageUrl);
    if (inlineImage) {
      parsed.images.push(inlineImage);
      return;
    }
    parsed.images.push({
      src: imageUrl,
      mediaType: 'image/*',
      source: 'stored',
      storagePath: effectiveSourcePath || undefined,
    });
    return;
  }

  if (Object.prototype.hasOwnProperty.call(node, 'stdout') || Object.prototype.hasOwnProperty.call(node, 'stderr')) {
    parseOutputEntry(node.stdout, parsed, textChunks, effectiveSourcePath);
    parseOutputEntry(node.stderr, parsed, textChunks, effectiveSourcePath);
    return;
  }

  if (Object.prototype.hasOwnProperty.call(node, 'content')) {
    parseOutputEntry(node.content, parsed, textChunks, effectiveSourcePath);
  }
  if (Object.prototype.hasOwnProperty.call(node, 'contents')) {
    parseOutputEntry(node.contents, parsed, textChunks, effectiveSourcePath);
  }
  if (Object.prototype.hasOwnProperty.call(node, 'output')) {
    parseOutputEntry(node.output, parsed, textChunks, effectiveSourcePath);
  }
  if (Object.prototype.hasOwnProperty.call(node, 'outputs')) {
    parseOutputEntry(node.outputs, parsed, textChunks, effectiveSourcePath);
  }
  if (Object.prototype.hasOwnProperty.call(node, 'items')) {
    parseOutputEntry(node.items, parsed, textChunks, effectiveSourcePath);
  }
}

export function parseCommandOutput(output?: string): ParsedCommandOutput {
  if (!output || output.trim().length === 0) {
    return {
      images: [],
      storagePaths: [],
    };
  }

  const buildParsedOutput = (entry: unknown, sourcePath?: string): ParsedCommandOutput => {
    const parsed: ParsedCommandOutput = {
      images: [],
      storagePaths: [],
    };
    const textChunks: string[] = [];
    parseOutputEntry(entry, parsed, textChunks, sourcePath);
    if (textChunks.length === 0) {
      return parsed;
    }
    const merged = textChunks.join('\n\n').trim();
    if (merged.length > 0) {
      parsed.text = formatCodeText(merged, detectCodeLanguage(merged));
    }
    parsed.storagePaths = [...new Set(parsed.storagePaths)];
    return parsed;
  };

  const interpreted = unwrapStructuredOutputNode(parseJsonLike(output));
  if (
    interpreted &&
    typeof interpreted === 'object' &&
    !Array.isArray(interpreted)
  ) {
    const record = interpreted as Record<string, unknown>;
    const hasTerminalShape =
      Object.prototype.hasOwnProperty.call(record, 'stdout') ||
      Object.prototype.hasOwnProperty.call(record, 'stderr') ||
      Object.prototype.hasOwnProperty.call(record, 'exit_code');
    if (hasTerminalShape) {
      const stdout = buildParsedOutput(record.stdout);
      const stderr = buildParsedOutput(record.stderr);
      return {
        text: stdout.text ?? stderr.text,
        stdoutText: stdout.text,
        stderrText: stderr.text,
        exitCode:
          typeof record.exit_code === 'number' ? record.exit_code : undefined,
        images: [...stdout.images, ...stderr.images],
        storagePaths: [...new Set([...stdout.storagePaths, ...stderr.storagePaths])],
      };
    }
  }

  const parsed = buildParsedOutput(interpreted);
  if (
    !parsed.text &&
    parsed.images.length === 0 &&
    parsed.storagePaths.length === 0
  ) {
    if (isLikelyBinaryTextPayload(output)) {
      return parsed;
    }
    return {
      ...parsed,
      text: formatCodeText(output, detectCodeLanguage(output)),
    };
  }
  return parsed;
}
