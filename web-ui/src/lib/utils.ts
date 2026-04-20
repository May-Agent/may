import { clsx, type ClassValue } from 'clsx';
import { twMerge } from 'tailwind-merge';

export function cn(...inputs: ClassValue[]): string {
  return twMerge(clsx(inputs));
}

export function formatDurationMs(ms: number): string {
  if (!Number.isFinite(ms) || ms < 0) return '—';
  if (ms < 1000) return `${Math.round(ms)}ms`;
  const seconds = ms / 1000;
  if (seconds < 60) return `${seconds.toFixed(seconds < 10 ? 1 : 0)}s`;
  const minutes = Math.floor(seconds / 60);
  const remSeconds = Math.round(seconds % 60);
  return `${minutes}m ${remSeconds}s`;
}

export function formatRelativeTime(iso: string | number | Date): string {
  const date = typeof iso === 'string' || typeof iso === 'number' ? new Date(iso) : iso;
  if (Number.isNaN(date.getTime())) return '';
  const diffMs = Date.now() - date.getTime();
  const diffSec = Math.round(diffMs / 1000);
  if (diffSec < 45) return 'just now';
  if (diffSec < 90) return '1 min ago';
  const diffMin = Math.round(diffSec / 60);
  if (diffMin < 45) return `${diffMin} min ago`;
  if (diffMin < 90) return '1 hour ago';
  const diffHr = Math.round(diffMin / 60);
  if (diffHr < 22) return `${diffHr} hours ago`;
  if (diffHr < 36) return 'yesterday';
  const diffDay = Math.round(diffHr / 24);
  if (diffDay < 25) return `${diffDay} days ago`;
  const diffMonth = Math.round(diffDay / 30);
  if (diffMonth < 11) return `${diffMonth} months ago`;
  const diffYear = Math.round(diffDay / 365);
  return `${diffYear} year${diffYear === 1 ? '' : 's'} ago`;
}
