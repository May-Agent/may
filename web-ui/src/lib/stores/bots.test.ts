import { describe, expect, it } from 'bun:test';
import { useBotsStore } from './bots';
import type { BotSummary } from '../api/bots';

const SAMPLE_BOT: BotSummary = {
  id: 'bot-1',
  agentId: 'amber-fox-pine',
  name: 'Assistant',
  persona: 'Helpful bridge',
  model: null,
  profileId: null,
  enabled: true,
  approvalPolicy: {
    unsafe: 'bot_triage',
    ssh: 'bot_triage',
    publish: 'bot_triage',
    workspace: 'bot_triage',
  },
  runState: 'idle',
  pendingInbox: 0,
  activeDelegates: 0,
  pendingApprovals: 0,
  createdAt: '2026-11-25T20:00:00Z',
  updatedAt: '2026-11-25T20:00:00Z',
};

describe('useBotsStore SSE reducers', () => {
  it('applies bot_state, bot_chat (deduped with CJK), bot_trace, and bot_approval frames', () => {
    useBotsStore.setState({
      bots: [SAMPLE_BOT],
      selectedBotId: SAMPLE_BOT.id,
      messages: {},
      trace: {},
      approvals: {},
    });

    useBotsStore.getState().applyBotStateEvent({
      bots: [
        {
          ...SAMPLE_BOT,
          runState: 'running',
          pendingInbox: 2,
          activeDelegates: 1,
          pendingApprovals: 1,
        },
      ],
    });
    expect(useBotsStore.getState().bots[0]?.runState).toBe('running');
    expect(useBotsStore.getState().bots[0]?.pendingApprovals).toBe(1);

    const chatMsg = {
      id: 10,
      botId: 'bot-1',
      role: 'bot' as const,
      content: '你好 / こんにちは / 안녕하세요',
      tsMs: 1700000000000,
      tsUtc: '2023-11-14T22:13:20Z',
    };
    useBotsStore.getState().applyBotChatEvent({ message: chatMsg });
    useBotsStore.getState().applyBotChatEvent({ message: chatMsg });
    expect(useBotsStore.getState().messages['bot-1']?.length).toBe(1);

    useBotsStore.getState().applyBotTraceEvent({
      entry: {
        id: 1,
        botId: 'bot-1',
        kind: 'text',
        content: 'internal reasoning trace',
        tsUtc: '2026-11-25T20:00:01Z',
      },
    });
    expect(useBotsStore.getState().trace['bot-1']?.length).toBe(1);

    useBotsStore.getState().applyBotApprovalEvent({
      approval: {
        requestId: 'req-1',
        botId: 'bot-1',
        requesterKind: 'delegate',
        requesterId: 'cedar-owl-brook',
        gate: 'publish',
        subject: 'Open upstream PR',
        reason: 'Ready for review',
        status: 'escalated',
        decisionNote: 'Needs human sign-off',
        createdAt: '2026-11-25T20:00:02Z',
        resolvedAt: null,
      },
    });
    expect(useBotsStore.getState().approvals['bot-1']?.[0]?.decisionNote).toBe(
      'Needs human sign-off',
    );
  });
});
