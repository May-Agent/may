import { useCallback } from 'react';
import { useNavigate } from 'react-router-dom';
import { toast } from 'sonner';
import { create } from 'zustand';
import { useSessionsStore } from './stores/sessions';
import { useUiStore } from './stores/ui';
import { profileSessionPath, useActiveProfile } from './profile/active';
import { translate } from './i18n';

/** Whether a new chat is being created, wherever it was started from. */
export const useNewChatStore = create<{ creating: boolean }>(() => ({ creating: false }));

/**
 * The one "new chat" action behind the sidebar button, the home screen,
 * the command palette and Cmd/Ctrl+N. A second request while one is in
 * flight is ignored.
 */
export function useNewChat() {
  const createNew = useSessionsStore((s) => s.createNew);
  const creating = useNewChatStore((s) => s.creating);
  const navigate = useNavigate();
  const { profileId } = useActiveProfile();

  const start = useCallback(async () => {
    if (!profileId || useNewChatStore.getState().creating) return;
    useNewChatStore.setState({ creating: true });
    try {
      const session = await createNew(profileId);
      useUiStore.getState().setSidebarSheetOpen(false);
      navigate(profileSessionPath(profileId, session.id));
    } catch (err) {
      toast.error(translate('errors.generic'), {
        description: err instanceof Error ? err.message : undefined,
      });
    } finally {
      useNewChatStore.setState({ creating: false });
    }
  }, [createNew, navigate, profileId]);

  return { start, creating, enabled: profileId !== null && profileId !== undefined };
}
