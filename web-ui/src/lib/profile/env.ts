import { Box, Laptop, type LucideIcon } from 'lucide-react';
import type { HomeEnvironment } from '@/lib/api/profiles';
import type { ProfileEnvironmentKind } from '@/lib/types';

export function envKindOf(env: HomeEnvironment): ProfileEnvironmentKind {
  return env.type;
}

export function envImageOf(env: HomeEnvironment): string {
  return env.type === 'container' ? env.image ?? '' : '';
}

export const ENV_ICON: Record<ProfileEnvironmentKind, LucideIcon> = {
  local: Laptop,
  container: Box,
};

/**
 * Derive a stable HSL accent colour from a profile id so each profile's
 * avatar/tint is consistent across sessions without needing user input.
 */
export function profileAccent(id: string): { hue: number; solid: string; soft: string } {
  let hash = 0;
  for (let i = 0; i < id.length; i += 1) {
    hash = (hash * 31 + id.charCodeAt(i)) | 0;
  }
  const hue = Math.abs(hash) % 360;
  return {
    hue,
    solid: `hsl(${hue} 70% 55%)`,
    soft: `hsl(${hue} 70% 55% / 0.14)`,
  };
}

export function profileInitial(name: string): string {
  const trimmed = name.trim();
  const cp = trimmed.codePointAt(0);
  if (cp === undefined) return '?';
  return String.fromCodePoint(cp).toUpperCase();
}
