/**
 * Extract the decoded prefix of a string-valued JSON field from a partial JSON
 * buffer. Designed for the streaming tool-call delta path where the LLM emits
 * an incomplete JSON object and we want to surface in-progress `description`
 * and `script` values as a typewriter effect.
 *
 * The function tolerates truncation at any point — including mid-escape — by
 * returning what has been decoded so far. It does not reject unknown fields
 * or validate JSON structure beyond what is required to locate the target
 * string value.
 *
 * Returns:
 *   - a decoded string (possibly empty) when the key is found
 *   - undefined when the key has not yet appeared in the buffer
 */
export function extractStringField(
  buffer: string,
  key: string,
): string | undefined {
  if (buffer.length === 0 || key.length === 0) {
    return undefined;
  }
  const keyToken = `"${key}"`;
  let searchFrom = 0;
  while (searchFrom < buffer.length) {
    const keyIndex = buffer.indexOf(keyToken, searchFrom);
    if (keyIndex < 0) {
      return undefined;
    }
    if (!isKeyBoundary(buffer, keyIndex, keyToken.length)) {
      searchFrom = keyIndex + keyToken.length;
      continue;
    }
    let cursor = keyIndex + keyToken.length;
    cursor = skipWhitespace(buffer, cursor);
    if (cursor >= buffer.length) {
      return undefined;
    }
    if (buffer.charCodeAt(cursor) !== CHAR_COLON) {
      searchFrom = keyIndex + keyToken.length;
      continue;
    }
    cursor = skipWhitespace(buffer, cursor + 1);
    if (cursor >= buffer.length) {
      return '';
    }
    if (buffer.charCodeAt(cursor) !== CHAR_QUOTE) {
      // Value started with something other than a string (null/number/object).
      // Not the kind of field we want to surface — keep looking in case the
      // same key appears later in a nested object (pathological, but safe).
      searchFrom = keyIndex + keyToken.length;
      continue;
    }
    return decodeJsonStringPrefix(buffer, cursor + 1);
  }
  return undefined;
}

const CHAR_BACKSLASH = 0x5c;
const CHAR_QUOTE = 0x22;
const CHAR_COLON = 0x3a;
const CHAR_SPACE = 0x20;
const CHAR_TAB = 0x09;
const CHAR_LF = 0x0a;
const CHAR_CR = 0x0d;
const CHAR_COMMA = 0x2c;
const CHAR_OPEN_BRACE = 0x7b;
const CHAR_OPEN_BRACKET = 0x5b;

function isWhitespaceCode(code: number): boolean {
  return (
    code === CHAR_SPACE ||
    code === CHAR_TAB ||
    code === CHAR_LF ||
    code === CHAR_CR
  );
}

function skipWhitespace(buffer: string, cursor: number): number {
  let i = cursor;
  while (i < buffer.length && isWhitespaceCode(buffer.charCodeAt(i))) {
    i += 1;
  }
  return i;
}

function isKeyBoundary(
  buffer: string,
  keyIndex: number,
  keyTokenLength: number,
): boolean {
  // Require that the `"key"` token appears where a JSON object key is
  // legal — i.e., preceded (after whitespace) by `{` or `,` or start of
  // buffer, and followed (after whitespace) by `:`. The post-check happens
  // in the caller; here we only validate the prefix side.
  let i = keyIndex - 1;
  while (i >= 0 && isWhitespaceCode(buffer.charCodeAt(i))) {
    i -= 1;
  }
  if (i < 0) {
    return true;
  }
  const code = buffer.charCodeAt(i);
  return (
    code === CHAR_OPEN_BRACE ||
    code === CHAR_COMMA ||
    code === CHAR_OPEN_BRACKET
  );
  // `keyTokenLength` is unused here but kept in the signature to document
  // the full key span that matched.
  void keyTokenLength;
}

function decodeJsonStringPrefix(buffer: string, start: number): string {
  let out = '';
  let i = start;
  while (i < buffer.length) {
    const code = buffer.charCodeAt(i);
    if (code === CHAR_QUOTE) {
      return out;
    }
    if (code !== CHAR_BACKSLASH) {
      out += buffer[i];
      i += 1;
      continue;
    }
    // Escape sequence
    if (i + 1 >= buffer.length) {
      // Backslash at EOF — treat the escape as incomplete and stop here
      // without consuming it; on the next delta the full escape will be
      // available.
      return out;
    }
    const escChar = buffer[i + 1];
    switch (escChar) {
      case '"':
        out += '"';
        i += 2;
        break;
      case '\\':
        out += '\\';
        i += 2;
        break;
      case '/':
        out += '/';
        i += 2;
        break;
      case 'n':
        out += '\n';
        i += 2;
        break;
      case 'r':
        out += '\r';
        i += 2;
        break;
      case 't':
        out += '\t';
        i += 2;
        break;
      case 'b':
        out += '\b';
        i += 2;
        break;
      case 'f':
        out += '\f';
        i += 2;
        break;
      case 'u': {
        if (i + 6 > buffer.length) {
          // Incomplete `\uXXXX` — stop, wait for more buffer.
          return out;
        }
        const hex = buffer.slice(i + 2, i + 6);
        if (!/^[0-9a-fA-F]{4}$/.test(hex)) {
          // Malformed escape — emit the backslash-u verbatim and continue.
          out += '\\u';
          i += 2;
          break;
        }
        out += String.fromCharCode(parseInt(hex, 16));
        i += 6;
        break;
      }
      default:
        // Unknown escape — pass through verbatim, advance past the backslash
        // so the next iteration picks up the following character normally.
        out += '\\';
        i += 1;
        break;
    }
  }
  return out;
}
