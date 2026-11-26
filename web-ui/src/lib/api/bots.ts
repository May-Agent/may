import { apiDelete, apiGet, apiPatch, apiPost } from './client';

export type GatePolicy = 'auto_approve' | 'bot_triage' | 'always_escalate';

export interface BotApprovalPolicy {
  unsafe: GatePolicy;
  ssh: GatePolicy;
  publish: GatePolicy;
  workspace: GatePolicy;
}

export type BotRunState = 'idle' | 'running' | 'disabled' | 'error';

export interface BotSummary {
  id: string;
  agentId: string;
  name: string;
  persona: string | null;
  model: string | null;
  profileId: string | null;
  enabled: boolean;
  approvalPolicy: BotApprovalPolicy;
  runState: BotRunState;
  pendingInbox: number;
  activeDelegates: number;
  pendingApprovals: number;
  createdAt: string;
  updatedAt: string;
}

export interface CreateBotRequest {
  name: string;
  persona?: string;
  model?: string;
  profileId?: string;
  enabled?: boolean;
  approvalPolicy?: BotApprovalPolicy;
}

export interface UpdateBotRequest {
  name?: string;
  persona?: string | null;
  model?: string | null;
  enabled?: boolean;
  approvalPolicy?: BotApprovalPolicy;
}

export interface BotChatMessage {
  id: number;
  botId: string;
  role: 'user' | 'bot';
  content: string;
  tsMs: number;
  tsUtc: string;
}

export interface BotTraceEntry {
  id: number;
  botId: string;
  kind: string;
  content: string;
  tsUtc: string;
}

export interface BotInboxEntry {
  id: number;
  botId: string;
  kind: string;
  senderId: string | null;
  senderName: string | null;
  body: string;
  tsMs: number;
  tsUtc: string;
  consumedAt: string | null;
}

export interface BotDelegateSummary {
  agentId: string;
  botId: string;
  name: string;
  definition: string;
  status: 'running' | 'stopped' | 'completed' | 'failed';
  workDir: string;
  grantedWorktrees: string[];
  activeCommand: string | null;
  elapsedMs: number;
  promptTokens: number;
  completionTokens: number;
  summary: string | null;
  createdAt: string;
  updatedAt: string;
}

export interface BotApprovalItem {
  requestId: string;
  botId: string;
  requesterKind: string;
  requesterId: string;
  gate: 'unsafe' | 'ssh' | 'publish' | 'workspace';
  subject: string;
  reason: string | null;
  status: 'pending_bot' | 'escalated' | 'approved' | 'denied';
  decisionNote: string | null;
  createdAt: string;
  resolvedAt: string | null;
}

export interface BotScheduleItem {
  id: string;
  botId: string;
  name: string;
  kind: 'once' | 'cron';
  cronExpression: string | null;
  runAt: string | null;
  prompt: string;
  enabled: boolean;
  nextFireAt: string | null;
  lastFiredAt: string | null;
}

export interface BotMemoryNode {
  path: string;
  name: string;
  isDir: boolean;
  content: string | null;
  children: BotMemoryNode[];
}

export interface BotMemoryCommit {
  sha: string;
  subject: string;
  message: string;
  author: string;
  timestamp: string;
}

export interface BotStateEventPayload {
  bots: BotSummary[];
}

export interface BotChatEventPayload {
  message: BotChatMessage;
}

export interface BotTraceEventPayload {
  entry: BotTraceEntry;
}

export interface BotApprovalEventPayload {
  approval: BotApprovalItem;
}

export interface BotMemoryEventPayload {
  botId: string;
  commitSha: string | null;
}

export const listBots = () => apiGet<BotSummary[]>('/bots');
export const createBot = (req: CreateBotRequest) => apiPost<BotSummary>('/bots', req);
export const updateBot = (botId: string, req: UpdateBotRequest) =>
  apiPatch<BotSummary>(`/bots/${encodeURIComponent(botId)}`, req);
export const deleteBot = (botId: string) =>
  apiDelete<{ ok: boolean }>(`/bots/${encodeURIComponent(botId)}`);
export const listBotMessages = (botId: string) =>
  apiGet<BotChatMessage[]>(`/bots/${encodeURIComponent(botId)}/messages`);
export const sendBotMessage = (botId: string, content: string) =>
  apiPost<BotChatMessage>(`/bots/${encodeURIComponent(botId)}/messages`, { content });
export const listBotTrace = (botId: string) =>
  apiGet<BotTraceEntry[]>(`/bots/${encodeURIComponent(botId)}/trace`);
export const listBotDelegates = (botId: string) =>
  apiGet<BotDelegateSummary[]>(`/bots/${encodeURIComponent(botId)}/delegates`);
export const stopBotDelegate = (botId: string, agentId: string) =>
  apiPost<BotDelegateSummary>(
    `/bots/${encodeURIComponent(botId)}/delegates/${encodeURIComponent(agentId)}/stop`,
  );
export const listBotApprovals = (botId: string) =>
  apiGet<BotApprovalItem[]>(`/bots/${encodeURIComponent(botId)}/approvals`);
export const resolveBotApproval = (
  botId: string,
  requestId: string,
  approved: boolean,
  reason?: string,
) =>
  apiPost<BotApprovalItem>(
    `/bots/${encodeURIComponent(botId)}/approvals/${encodeURIComponent(requestId)}`,
    { approved, reason },
  );
export const listBotSchedules = (botId: string) =>
  apiGet<BotScheduleItem[]>(`/bots/${encodeURIComponent(botId)}/schedules`);
export const deleteBotSchedule = (botId: string, scheduleId: string) =>
  apiDelete<{ ok: boolean }>(
    `/bots/${encodeURIComponent(botId)}/schedules/${encodeURIComponent(scheduleId)}`,
  );
export const getBotMemory = (botId: string) =>
  apiGet<BotMemoryNode>(`/bots/${encodeURIComponent(botId)}/memory`);
export const getBotMemoryHistory = (botId: string) =>
  apiGet<BotMemoryCommit[]>(`/bots/${encodeURIComponent(botId)}/memory/history`);
