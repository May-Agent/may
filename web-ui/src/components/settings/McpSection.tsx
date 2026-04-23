import { useEffect, useState } from 'react';
import { useForm, useWatch } from 'react-hook-form';
import { zodResolver } from '@hookform/resolvers/zod';
import { z } from 'zod';
import { Plus, Trash2, RotateCw } from 'lucide-react';
import { toast } from 'sonner';
import { Button } from '@/components/ui/button';
import { SectionHeader } from './SectionHeader';
import { useTranslation } from '@/lib/i18n';
import {
  addMcpServer,
  listMcpServers,
  reconnectMcpServer,
  removeMcpServer,
  type McpServerInfo,
} from '@/lib/api/mcp';

const schema = z.discriminatedUnion('kind', [
  z.object({
    kind: z.literal('process'),
    name: z.string().trim().min(1),
    command: z.string().trim().min(1),
    args: z.string().trim(),
  }),
  z.object({
    kind: z.literal('url'),
    name: z.string().trim().min(1),
    url: z.string().trim().url(),
  }),
]);
type FormValues = z.infer<typeof schema>;

export function McpSection() {
  const { t } = useTranslation();
  const [items, setItems] = useState<McpServerInfo[]>([]);
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState(false);

  const form = useForm<FormValues>({
    resolver: zodResolver(schema),
    defaultValues: { kind: 'process', name: '', command: '', args: '' } as FormValues,
  });
  const kind = useWatch({ control: form.control, name: 'kind' });

  const refresh = async () => {
    setLoading(true);
    try {
      setItems(await listMcpServers());
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => {
    refresh();
  }, []);

  const onSubmit = form.handleSubmit(async (values) => {
    try {
      if (values.kind === 'process') {
        await addMcpServer({
          kind: 'process',
          name: values.name,
          command: values.command,
          args: values.args ? values.args.split(/\s+/).filter(Boolean) : [],
        });
      } else {
        await addMcpServer({ kind: 'url', name: values.name, url: values.url });
      }
      form.reset({ kind: values.kind, name: '', command: '', args: '', url: '' } as FormValues);
      await refresh();
    } catch (err) {
      toast.error(t('errors.generic'), {
        description: err instanceof Error ? err.message : undefined,
      });
    }
  });

  const handleRemove = async (n: string) => {
    setBusy(true);
    try {
      await removeMcpServer(n);
      await refresh();
    } catch (err) {
      toast.error(t('errors.generic'), {
        description: err instanceof Error ? err.message : undefined,
      });
    } finally {
      setBusy(false);
    }
  };

  const handleReconnect = async (n: string) => {
    setBusy(true);
    try {
      await reconnectMcpServer(n);
      await refresh();
    } catch (err) {
      toast.error(t('errors.generic'), {
        description: err instanceof Error ? err.message : undefined,
      });
    } finally {
      setBusy(false);
    }
  };

  const setKind = (k: 'process' | 'url') => {
    form.reset({ kind: k, name: '', command: '', args: '', url: '' } as FormValues);
  };

  const submitting = form.formState.isSubmitting;

  return (
    <section>
      <SectionHeader title={t('settings.mcp.title')} description={t('settings.mcp.description')} />

      <form onSubmit={onSubmit} className="mb-4 rounded-md border border-border p-3">
        <div className="mb-2 flex gap-1.5">
          <Button
            size="sm"
            type="button"
            variant={kind === 'process' ? 'default' : 'outline'}
            onClick={() => setKind('process')}
          >
            {t('settings.mcp.process')}
          </Button>
          <Button
            size="sm"
            type="button"
            variant={kind === 'url' ? 'default' : 'outline'}
            onClick={() => setKind('url')}
          >
            {t('settings.mcp.url')}
          </Button>
        </div>
        <div className="grid gap-2">
          <input
            {...form.register('name')}
            placeholder="name"
            className="rounded-md border border-border bg-background px-3 py-1.5 text-sm outline-none focus:ring-2 focus:ring-ring"
          />
          {kind === 'process' ? (
            <>
              <input
                {...form.register('command')}
                placeholder="command"
                className="rounded-md border border-border bg-background px-3 py-1.5 text-sm font-mono outline-none focus:ring-2 focus:ring-ring"
              />
              <input
                {...form.register('args')}
                placeholder="args (space-separated)"
                className="rounded-md border border-border bg-background px-3 py-1.5 text-sm font-mono outline-none focus:ring-2 focus:ring-ring"
              />
            </>
          ) : (
            <input
              {...form.register('url')}
              placeholder="https://…"
              className="rounded-md border border-border bg-background px-3 py-1.5 text-sm font-mono outline-none focus:ring-2 focus:ring-ring"
            />
          )}
          <Button type="submit" disabled={submitting}>
            <Plus className="size-4" />
            {t('settings.mcp.add')}
          </Button>
        </div>
      </form>

      {loading ? (
        <div className="py-6 text-center text-sm text-muted-foreground">{t('common.loading')}</div>
      ) : items.length === 0 ? (
        <div className="py-6 text-center text-sm text-muted-foreground">{t('settings.mcp.empty')}</div>
      ) : (
        <ul className="space-y-1">
          {items.map((s) => (
            <li
              key={s.name}
              className="flex items-center justify-between rounded-md border border-border px-3 py-2 text-sm"
            >
              <div className="min-w-0">
                <div className="truncate font-medium">{s.name}</div>
                <div className="truncate text-xs text-muted-foreground">
                  {s.kind === 'process'
                    ? `${s.command ?? ''} ${(s.args ?? []).join(' ')}`
                    : s.url ?? ''}
                  {s.status ? ` · ${s.status}` : ''}
                </div>
              </div>
              <div className="flex gap-1">
                <Button
                  size="icon"
                  variant="ghost"
                  disabled={busy}
                  onClick={() => handleReconnect(s.name)}
                  aria-label={t('settings.mcp.reconnect')}
                >
                  <RotateCw className="size-4" />
                </Button>
                <Button
                  size="icon"
                  variant="ghost"
                  disabled={busy}
                  onClick={() => handleRemove(s.name)}
                  aria-label={t('common.remove')}
                >
                  <Trash2 className="size-4" />
                </Button>
              </div>
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}
