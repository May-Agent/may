import { CircleX } from 'lucide-react';
import { Button } from '@/components/ui/button';

interface BackendDownScreenProps {
  reason: string | null;
  onRetry: () => void;
}

/**
 * Full-viewport error state shown when the frontend cannot reach the
 * May server. Deliberately loud and high-contrast so the user knows
 * the UI is not in a normal state and there's a clear next step.
 */
export function BackendDownScreen({ reason, onRetry }: BackendDownScreenProps) {
  return (
    <div className="flex flex-1 min-h-0 w-full min-w-0 items-center justify-center overflow-hidden bg-[#8f2020] px-6 py-10 text-red-50 md:px-10">
      <div className="w-full max-w-3xl text-center">
        <div className="mx-auto mb-6 flex h-16 w-16 items-center justify-center rounded-full border border-red-50/18">
          <CircleX className="h-8 w-8 text-red-50/92" />
        </div>
        <h2 className="text-3xl font-semibold tracking-[-0.03em] text-red-50">
          Backend unavailable
        </h2>
        <p className="mx-auto mt-4 max-w-xl text-base leading-7 text-red-50/76">
          The UI cannot reach the May server right now.
        </p>
        <div className="mx-auto mt-8 max-w-2xl rounded-3xl bg-red-950/16 px-6 py-4 text-sm leading-7 text-red-50/78 shadow-[inset_0_1px_0_rgba(255,255,255,0.06)] backdrop-blur-sm">
          {reason}
        </div>
        <Button
          variant="secondary"
          className="mt-8 h-10 rounded-full border-0 bg-red-50 text-[#8f2020] hover:bg-white"
          onClick={onRetry}
        >
          Retry
        </Button>
      </div>
    </div>
  );
}
