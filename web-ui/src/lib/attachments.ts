// Attachment file handling — pure utilities for classification, reading,
// and building Attachment objects from various input sources.

import type { Attachment } from '@/types';

const TEXT_PREVIEW_MAX_LENGTH = 200;

/**
 * Classify a File into an Attachment type category based on mime type
 * and extension. Falls back to 'file' for unknown kinds.
 */
export function classifyFile(file: File): Attachment['type'] {
  if (file.type.startsWith('image/')) return 'image';
  if (file.type === 'application/pdf') return 'pdf';
  if (
    file.type ===
      'application/vnd.openxmlformats-officedocument.wordprocessingml.document' ||
    file.name.endsWith('.docx')
  ) {
    return 'document';
  }
  if (
    file.type === 'text/plain' ||
    file.name.endsWith('.txt') ||
    file.name.endsWith('.md')
  ) {
    return 'document';
  }
  return 'file';
}

/** Read a File as a UTF-8 text string. */
export function readFileAsText(file: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => resolve(reader.result as string);
    reader.onerror = () =>
      reject(
        new Error(
          `failed to read text file ${file.name}: ${
            reader.error?.message ?? 'unknown error'
          }`,
        ),
      );
    reader.readAsText(file);
  });
}

/** Read a File as a data URL (base64-encoded). */
export function readFileAsDataUrl(file: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () =>
      resolve(typeof reader.result === 'string' ? reader.result : '');
    reader.onerror = () =>
      reject(
        new Error(
          `failed to read file ${file.name}: ${
            reader.error?.message ?? 'unknown error'
          }`,
        ),
      );
    reader.readAsDataURL(file);
  });
}

/** Lazy-load mammoth and extract raw text from a .docx file. */
export async function readDocxAsText(file: File): Promise<string> {
  try {
    const mammoth = await import('mammoth');
    const arrayBuffer = await file.arrayBuffer();
    const result = await mammoth.default.extractRawText({ arrayBuffer });
    return result.value;
  } catch (error) {
    throw new Error(
      `failed to parse DOCX ${file.name}: ${
        error instanceof Error ? error.message : String(error)
      }`,
    );
  }
}

/**
 * Convert a single File into a fully-populated Attachment, handling
 * text extraction for document types and previews where applicable.
 */
export async function buildAttachmentFromFile(
  file: File,
): Promise<Attachment> {
  const type = classifyFile(file);
  const attachment: Attachment = {
    id: crypto.randomUUID(),
    name: file.name,
    type,
    url: URL.createObjectURL(file),
    uploadDataUrl: await readFileAsDataUrl(file),
  };

  if (type === 'document') {
    const text = file.name.endsWith('.docx')
      ? await readDocxAsText(file)
      : await readFileAsText(file);
    attachment.textContent = text;
    attachment.textPreview = text.slice(0, TEXT_PREVIEW_MAX_LENGTH);
  }

  return attachment;
}

/**
 * Build a lightweight Attachment from a pasted File (no document
 * text extraction — pasted files are generally images or small blobs).
 */
export async function buildAttachmentFromPastedFile(
  file: File,
): Promise<Attachment> {
  const type = classifyFile(file);
  return {
    id: crypto.randomUUID(),
    name: file.name || (type === 'image' ? 'Pasted Image' : 'Pasted File'),
    type,
    url: URL.createObjectURL(file),
    uploadDataUrl: await readFileAsDataUrl(file),
  };
}

/** Build an Attachment wrapping a pasted long-text block. */
export function buildAttachmentFromPastedText(text: string): Attachment {
  return {
    id: crypto.randomUUID(),
    name: 'Pasted Text',
    type: 'document',
    url: '',
    textContent: text,
    textPreview: text.slice(0, TEXT_PREVIEW_MAX_LENGTH),
  };
}
