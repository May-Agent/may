import { apiDelete, apiGet, apiPost } from './client';

export interface SkillInfo {
  dir: string;
  name: string;
  description?: string | null;
  source?: string | null;
}

export function listSkills(profileId?: string): Promise<SkillInfo[]> {
  const qs = profileId ? `?profileId=${encodeURIComponent(profileId)}` : '';
  return apiGet<SkillInfo[]>(`/skills${qs}`);
}

export function installSkill(repo: string, profileId?: string): Promise<SkillInfo[]> {
  const qs = profileId ? `?profileId=${encodeURIComponent(profileId)}` : '';
  return apiPost<SkillInfo[]>(`/skills/install${qs}`, { repo });
}

export function uninstallSkill(dir: string, profileId?: string): Promise<void> {
  const qs = profileId ? `?profileId=${encodeURIComponent(profileId)}` : '';
  return apiDelete<void>(`/skills/${encodeURIComponent(dir)}${qs}`);
}
