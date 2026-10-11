import { apiGet } from './client';

export interface ModelInfoItem {
  id: string;
  modelId: string;
  providerId: string;
  nickname: string;
  contextWindow?: number | null;
  tier?: string | null;
}

export interface ModelsResponse {
  models: string[];
  details: ModelInfoItem[];
}

export interface ProviderModelsGroup {
  providerId: string;
  models: string[];
  details: ModelInfoItem[];
}

export interface ProviderModelsResponse {
  providers: ProviderModelsGroup[];
}

export function listModels(): Promise<ModelsResponse> {
  return apiGet<ModelsResponse>('/models');
}

export function listModelsByProvider(): Promise<ProviderModelsResponse> {
  return apiGet<ProviderModelsResponse>('/models/by-provider');
}
