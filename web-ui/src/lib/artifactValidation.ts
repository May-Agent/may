export interface ArtifactValidationMessage {
  source: 'may-artifact-runtime';
  version: 1;
  artifactType: string;
  diagnostics: string[];
}

export function parseArtifactValidationMessage(
  value: unknown,
  expectedArtifactType: string,
): ArtifactValidationMessage | null {
  if (!value || typeof value !== 'object') return null;
  const message = value as Partial<ArtifactValidationMessage>;
  if (
    message.source !== 'may-artifact-runtime' ||
    message.version !== 1 ||
    message.artifactType !== expectedArtifactType ||
    !Array.isArray(message.diagnostics) ||
    !message.diagnostics.every((diagnostic) => typeof diagnostic === 'string')
  ) {
    return null;
  }
  return message as ArtifactValidationMessage;
}
