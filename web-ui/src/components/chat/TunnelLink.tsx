import { useEffect, useState } from 'react';
import { ExternalLink } from 'lucide-react';
import { fetchTunnel } from '@/lib/api/tunnels';

interface TunnelLinkProps {
  sessionId: string;
  id: string;
}

/**
 * Renders `<tunnel id="…"/>` as a link to `/tunnel/<session>/<id>/`, resolved
 * relative to the current origin — so it works however the user reaches May
 * (localhost, tailnet, LAN).
 */
export function TunnelLink({ sessionId, id }: TunnelLinkProps) {
  const [target, setTarget] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    fetchTunnel(sessionId, id)
      .then((info) => {
        if (!cancelled) setTarget(info.target);
      })
      .catch(() => {
        if (!cancelled) setTarget(null);
      });
    return () => {
      cancelled = true;
    };
  }, [sessionId, id]);

  const href = `/tunnel/${encodeURIComponent(sessionId)}/${encodeURIComponent(id)}/`;
  return (
    <a
      href={href}
      target="_blank"
      rel="noreferrer"
      className="inline-flex items-center gap-1.5 rounded-md border border-border bg-muted/40 px-2 py-1 text-xs font-mono text-foreground no-underline hover:bg-muted"
      title={target ?? href}
    >
      <ExternalLink className="size-3.5 shrink-0" />
      {target ?? href}
    </a>
  );
}
