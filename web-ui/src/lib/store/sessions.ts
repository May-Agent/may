import type { StateCreator } from 'zustand';
import type { Session, Message, Attachment } from '@/types';
import * as api from '@/lib/api';
import { buildModelCatalog, type Model } from '@/lib/constants';
import { useRuntimeStore } from '@/lib/runtimeStore';
import type { AppState } from './index';
import {
  buildUiConfigState,
  sortSessions,
  backendUnavailableReasonFromError,
  errorMessage,
  isContainerProfile,
  PREVIEW_MAX_LENGTH,
  TITLE_MAX_LENGTH,
} from './shared';

export interface SessionsSlice {
  sessions: Session[];
  currentSessionId: string | null;
  models: Model[];
  modelsLoaded: boolean;
  loaded: boolean;

  loadInitialData: () => Promise<void>;
  createSession: (model?: string, profileIdOverride?: string | null) => Promise<string>;
  setCurrentSession: (sessionId: string | null) => void;
  addMessage: (
    sessionId: string,
    content: string,
    role: 'user' | 'assistant',
    attachments?: Attachment[],
  ) => void;
  deleteSession: (sessionId: string) => Promise<void>;
  updateSessionTitle: (sessionId: string, title: string) => Promise<void>;
  updateSessionModel: (sessionId: string, model: string) => Promise<void>;
  setSessionPlanState: (sessionId: string, planState: import('@/types').SessionPlanState) => void;
  updateSessionReasoningEffort: (sessionId: string, effort?: string) => void;
  replaceSessionMessages: (sessionId: string, messages: Message[]) => void;
  searchSessions: (query: string) => Session[];
  deleteAllSessions: () => Promise<void>;
}

export const createSessionsSlice: StateCreator<AppState, [], [], SessionsSlice> = (set, get) => ({
  sessions: [],
  currentSessionId: null,
  models: [],
  modelsLoaded: false,
  loaded: false,

  loadInitialData: async () => {
    if (get().loaded) return;

    try {
      const bootstrap = await api.fetchBootstrap();
      const {
        sessions,
        profiles,
        defaultProfileId,
        config,
        defaultProfileSessions,
        defaultProfileSessionsError,
        defaultProfileSkills,
      } = bootstrap;

      const sorted = sortSessions(sessions);
      useRuntimeStore.getState().applyRuntimes(
        sorted.map((session) => ({
          sessionId: session.id,
          runtime: session.runtime,
        })),
      );
      const defaultProfile =
        profiles.find((profile) => profile.id === defaultProfileId)
        ?? profiles[0]
        ?? null;
      const isDefaultContainerProfile =
        defaultProfile != null
          && isContainerProfile(defaultProfile.id, profiles);
      const profileAccessBlockedReason =
        isDefaultContainerProfile
          ? (defaultProfileSessionsError?.message ?? null)
          : null;
      const profileAccessBlockedCode =
        isDefaultContainerProfile
          ? (defaultProfileSessionsError?.code ?? null)
          : null;
      const backendUnavailableReason =
        !isDefaultContainerProfile && defaultProfileSessionsError
          ? defaultProfileSessionsError.message
          : null;
      set({
        sessions: sorted,
        ...buildUiConfigState(config),
        profiles,
        currentProfileId: defaultProfile?.id ?? null,
        profileSessions: defaultProfileSessions ?? null,
        profileSessionsLoading: false,
        profileAccessBlockedReason,
        profileAccessBlockedCode,
        backendUnavailableReason,
        skills: defaultProfileSkills,
        loaded: true,
        currentSessionId: null,
      });

      void api.fetchModels().then((modelPayload) => {
        set({
          models: buildModelCatalog(modelPayload.models, modelPayload.details),
          modelsLoaded: true,
        });
      }).catch((error) => {
        console.error('failed to load models during app bootstrap', error);
      });
    } catch (error) {
      // Keep loaded: false so a later refresh can retry bootstrap.
      set({
        backendUnavailableReason:
          backendUnavailableReasonFromError(error) ?? errorMessage(error),
        profileSessionsLoading: false,
      });
    }
  },

  createSession: async (model, profileIdOverride) => {
    const profileId = profileIdOverride ?? get().currentProfileId;
    const blockedReason = get().profileAccessBlockedReason;
    const backendUnavailableReason = get().backendUnavailableReason;
    if (backendUnavailableReason) {
      throw new Error(backendUnavailableReason);
    }
    if (
      isContainerProfile(profileId, get().profiles) &&
      blockedReason
    ) {
      throw new Error(blockedReason);
    }
    const session = await api.createSession(model, profileId ?? undefined);
    set((state) => ({
      sessions: sortSessions([session, ...state.sessions]),
      currentSessionId: session.id,
    }));
    return session.id;
  },

  setCurrentSession: (sessionId) => {
    set({ currentSessionId: sessionId });
    if (!sessionId) return;
    const session = get().sessions.find((s) => s.id === sessionId);
    if (session && session.messages.length === 0 && session.preview !== 'New conversation started') {
      void api.fetchSession(sessionId).then(
        (full) => {
          set((state) => ({
            sessions:
              state.currentSessionId === sessionId
                ? state.sessions.map((s) =>
                    s.id === sessionId ? { ...s, messages: full.messages } : s,
                  )
                : state.sessions,
          }));
        },
      ).catch((error) => {
        const backendReason = backendUnavailableReasonFromError(error);
        if (backendReason) {
          set({ backendUnavailableReason: backendReason });
        }
        console.error(`failed to load session ${sessionId} messages`, error);
      });
    }
  },

  addMessage: (sessionId, content, role, attachments = []) => {
    const newMessage: Message = {
      id: crypto.randomUUID(),
      role,
      content,
      timestamp: new Date().toISOString(),
      attachments,
    };

    set((state) => ({
      sessions: sortSessions(
        state.sessions.map((session) =>
          session.id === sessionId
            ? {
                ...session,
                messages: [...session.messages, newMessage],
                lastModified: new Date().toISOString(),
                preview:
                  content.substring(0, PREVIEW_MAX_LENGTH) + (content.length > PREVIEW_MAX_LENGTH ? '...' : ''),
                title:
                  session.messages.length === 0 && role === 'user'
                    ? content.substring(0, TITLE_MAX_LENGTH)
                    : session.title,
              }
            : session,
        ),
      ),
    }));
  },

  deleteSession: async (sessionId) => {
    await api.removeSession(sessionId);
    useRuntimeStore.getState().removeSession(sessionId);
    set((state) => {
      const sessions = state.sessions.filter((s) => s.id !== sessionId);
      const nextId =
        state.currentSessionId === sessionId
          ? (sessions[0]?.id ?? null)
          : state.currentSessionId;
      return { sessions, currentSessionId: nextId };
    });
  },

  updateSessionTitle: async (sessionId, title) => {
    const updated = await api.updateSession(sessionId, { title });
    set((state) => ({
      sessions: state.sessions.map((s) =>
        s.id === sessionId ? { ...s, title: updated.title } : s,
      ),
    }));
  },

  updateSessionModel: async (sessionId, model) => {
    const previousModel = get().sessions.find((s) => s.id === sessionId)?.model;
    if (previousModel === model) {
      return;
    }

    set((state) => ({
      sessions: state.sessions.map((s) =>
        s.id === sessionId ? { ...s, model } : s,
      ),
    }));

    try {
      const updated = await api.updateSession(sessionId, { model });
      set((state) => ({
        sessions: state.sessions.map((s) =>
          s.id === sessionId ? { ...s, model: updated.model } : s,
        ),
      }));
    } catch (error) {
      // Always restore the prior value on failure, even if it was empty.
      // Leaving the optimistic write in place would drift the UI from
      // backend truth the moment the next session refresh arrives.
      set((state) => ({
        sessions: state.sessions.map((s) =>
          s.id === sessionId ? { ...s, model: previousModel ?? '' } : s,
        ),
      }));
      throw error;
    }
  },

  setSessionPlanState: (sessionId, planState) => {
    set((state) => ({
      sessions: state.sessions.map((session) =>
        session.id === sessionId ? { ...session, planState } : session,
      ),
    }));
  },

  updateSessionReasoningEffort: (sessionId, effort) => {
    set((state) => ({
      sessions: state.sessions.map((s) =>
        s.id === sessionId ? { ...s, reasoningEffort: effort } : s,
      ),
    }));
  },

  replaceSessionMessages: (sessionId, messages) => {
    set((state) => ({
      sessions: sortSessions(
        state.sessions.map((s) =>
          s.id === sessionId
            ? {
                ...s,
                messages,
                lastModified: new Date().toISOString(),
                preview: messages.length
                  ? messages[messages.length - 1].content.slice(0, PREVIEW_MAX_LENGTH)
                  : s.preview,
              }
            : s,
        ),
      ),
    }));
  },

  searchSessions: (query) => {
    const lower = query.toLowerCase();
    const sessions = get().sessions;
    if (!query) return sessions;
    return sessions.filter(
      (s) =>
        s.title.toLowerCase().includes(lower) ||
        s.preview.toLowerCase().includes(lower),
    );
  },

  deleteAllSessions: async () => {
    await api.deleteAllSessions();
    const sessions = await api.fetchSessions();
    const sorted = sortSessions(sessions);
    set({
      sessions: sorted,
      currentSessionId: sorted[0]?.id ?? null,
    });
  },
});
