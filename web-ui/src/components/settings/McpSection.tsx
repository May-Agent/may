import { useState } from "react";
import {
  Globe,
  PlugZap,
  Plus,
  RotateCcw,
  TerminalSquare,
  Trash2,
} from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { useStore } from "@/lib/store";
import type { McpServerCreateRequest, McpServerInfo } from "@/lib/api";

type DraftSourceKind = McpServerCreateRequest["kind"];

interface DraftArg {
  id: string;
  value: string;
}

interface DraftEnvVar {
  id: string;
  key: string;
  value: string;
}

function nextDraftId(): string {
  const randomUuid = globalThis.crypto?.randomUUID;
  if (typeof randomUuid === "function") {
    return randomUuid.call(globalThis.crypto);
  }
  return [Date.now().toString(36), Math.random().toString(36).slice(2)].join(
    "-",
  );
}

function createDraftArg(value = ""): DraftArg {
  return { id: nextDraftId(), value };
}

function createDraftEnvVar(): DraftEnvVar {
  return { id: nextDraftId(), key: "", value: "" };
}

function resetProcessDraft() {
  return {
    command: "",
    args: [] as DraftArg[],
    env: [] as DraftEnvVar[],
  };
}

function mcpSourceLabel(source: McpServerInfo["source"]): string {
  switch (source.kind) {
    case "url":
      return source.url;
    case "process":
      return [source.command, ...source.args].join(" ").trim();
    case "unknown":
      return "Unknown";
  }
}

function mcpStatusLabel(status: McpServerInfo["status"]): string {
  switch (status.state) {
    case "disconnected":
      return "Disconnected";
    case "connecting":
      return "Connecting...";
    case "connected":
      return `Connected (${status.toolCount} tools)`;
    case "error":
      return `Error: ${status.message}`;
  }
}

function ProcessArgsEditor({
  args,
  onChange,
}: {
  args: DraftArg[];
  onChange: (value: DraftArg[]) => void;
}) {
  return (
    <div className="space-y-2">
      <div className="flex items-center justify-between">
        <div className="text-xs uppercase tracking-wide text-muted-foreground">
          Arguments
        </div>
        <Button
          type="button"
          variant="outline"
          size="sm"
          onClick={() => onChange([...args, createDraftArg()])}
        >
          <Plus className="mr-2 h-4 w-4" />
          Add Arg
        </Button>
      </div>
      {args.length === 0 ? (
        <div className="rounded-md border border-dashed px-3 py-2 text-xs text-muted-foreground">
          No arguments. Each row becomes one exact argv entry.
        </div>
      ) : (
        <div className="space-y-2">
          {args.map((arg) => (
            <div key={arg.id} className="flex items-center gap-2">
              <Input
                value={arg.value}
                onChange={(event) =>
                  onChange(
                    args.map((entry) =>
                      entry.id === arg.id
                        ? { ...entry, value: event.target.value }
                        : entry,
                    ),
                  )
                }
                placeholder="argv item"
              />
              <Button
                type="button"
                variant="ghost"
                size="icon"
                onClick={() =>
                  onChange(args.filter((entry) => entry.id !== arg.id))
                }
              >
                <Trash2 className="h-4 w-4" />
              </Button>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

function ProcessEnvEditor({
  env,
  onChange,
}: {
  env: DraftEnvVar[];
  onChange: (value: DraftEnvVar[]) => void;
}) {
  return (
    <div className="space-y-2">
      <div className="flex items-center justify-between">
        <div className="text-xs uppercase tracking-wide text-muted-foreground">
          Environment
        </div>
        <Button
          type="button"
          variant="outline"
          size="sm"
          onClick={() => onChange([...env, createDraftEnvVar()])}
        >
          <Plus className="mr-2 h-4 w-4" />
          Add Variable
        </Button>
      </div>
      {env.length === 0 ? (
        <div className="rounded-md border border-dashed px-3 py-2 text-xs text-muted-foreground">
          No environment variables.
        </div>
      ) : (
        <div className="space-y-2">
          {env.map((entry) => (
            <div
              key={entry.id}
              className="grid gap-2 md:grid-cols-[minmax(0,1fr)_minmax(0,1fr)_40px]"
            >
              <Input
                value={entry.key}
                onChange={(event) =>
                  onChange(
                    env.map((candidate) =>
                      candidate.id === entry.id
                        ? { ...candidate, key: event.target.value }
                        : candidate,
                    ),
                  )
                }
                placeholder="KEY"
              />
              <Input
                value={entry.value}
                onChange={(event) =>
                  onChange(
                    env.map((candidate) =>
                      candidate.id === entry.id
                        ? { ...candidate, value: event.target.value }
                        : candidate,
                    ),
                  )
                }
                placeholder="value"
              />
              <Button
                type="button"
                variant="ghost"
                size="icon"
                onClick={() =>
                  onChange(env.filter((candidate) => candidate.id !== entry.id))
                }
              >
                <Trash2 className="h-4 w-4" />
              </Button>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

export function McpSection() {
  const { mcpServers, addMcpServer, removeMcpServer, reconnectMcpServer } =
    useStore();
  const [sourceKind, setSourceKind] = useState<DraftSourceKind>("process");
  const [mcpName, setMcpName] = useState("");
  const [mcpUrl, setMcpUrl] = useState("");
  const [processDraft, setProcessDraft] = useState(resetProcessDraft);

  const trimmedName = mcpName.trim();
  const trimmedCommand = processDraft.command.trim();
  const trimmedUrl = mcpUrl.trim();
  const canSubmit =
    trimmedName.length > 0 &&
    (sourceKind === "process"
      ? trimmedCommand.length > 0
      : trimmedUrl.length > 0);

  async function handleAddServer() {
    if (!canSubmit) {
      return;
    }

    if (sourceKind === "process") {
      const args = processDraft.args
        .map((entry) => entry.value.trim())
        .filter((value) => value.length > 0);
      const env = Object.fromEntries(
        processDraft.env
          .map((entry) => [entry.key.trim(), entry.value] as const)
          .filter(([key]) => key.length > 0),
      );

      await addMcpServer({
        kind: "process",
        name: trimmedName,
        command: trimmedCommand,
        args,
        env,
      });
    } else {
      await addMcpServer({
        kind: "url",
        name: trimmedName,
        url: trimmedUrl,
      });
    }

    setMcpName("");
    setMcpUrl("");
    setProcessDraft(resetProcessDraft());
    setSourceKind("process");
  }

  return (
    <section className="space-y-4">
      <div className="flex items-center gap-2 text-sm font-medium uppercase tracking-wide text-muted-foreground">
        <PlugZap className="h-4 w-4" />
        MCP Servers
      </div>

      <div className="space-y-4 rounded-xl border bg-muted/20 p-4">
        <div className="grid gap-3 md:grid-cols-[minmax(0,1fr)_220px]">
          <Input
            value={mcpName}
            onChange={(event) => setMcpName(event.target.value)}
            placeholder="server name"
          />
          <Select
            value={sourceKind}
            onValueChange={(value: DraftSourceKind) => setSourceKind(value)}
          >
            <SelectTrigger>
              <SelectValue placeholder="Source kind" />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="process">Process command</SelectItem>
              <SelectItem value="url">HTTP endpoint</SelectItem>
            </SelectContent>
          </Select>
        </div>

        {sourceKind === "process" ? (
          <div className="space-y-4">
            <div className="grid gap-2">
              <div className="flex items-center gap-2 text-xs uppercase tracking-wide text-muted-foreground">
                <TerminalSquare className="h-4 w-4" />
                Process Source
              </div>
              <Input
                value={processDraft.command}
                onChange={(event) =>
                  setProcessDraft((current) => ({
                    ...current,
                    command: event.target.value,
                  }))
                }
                placeholder="command"
              />
            </div>
            <ProcessArgsEditor
              args={processDraft.args}
              onChange={(args) =>
                setProcessDraft((current) => ({ ...current, args }))
              }
            />
            <ProcessEnvEditor
              env={processDraft.env}
              onChange={(env) =>
                setProcessDraft((current) => ({ ...current, env }))
              }
            />
          </div>
        ) : (
          <div className="space-y-2">
            <div className="flex items-center gap-2 text-xs uppercase tracking-wide text-muted-foreground">
              <Globe className="h-4 w-4" />
              URL Source
            </div>
            <Input
              value={mcpUrl}
              onChange={(event) => setMcpUrl(event.target.value)}
              placeholder="https://example.com/mcp"
            />
          </div>
        )}

        <Button onClick={() => void handleAddServer()} disabled={!canSubmit}>
          <Plus className="mr-2 h-4 w-4" />
          Add MCP Server
        </Button>
      </div>

      <div className="space-y-2">
        {mcpServers.map((server) => (
          <div
            key={server.name}
            className="flex items-center justify-between rounded-lg border px-3 py-3"
          >
            <div className="space-y-1">
              <div className="font-medium">{server.name}</div>
              <div className="text-xs text-muted-foreground">
                {mcpSourceLabel(server.source)}
              </div>
              <div className="text-xs">{mcpStatusLabel(server.status)}</div>
            </div>
            <div className="flex gap-2">
              <Button
                variant="outline"
                size="icon"
                onClick={() => reconnectMcpServer(server.name)}
              >
                <RotateCcw className="h-4 w-4" />
              </Button>
              <Button
                variant="ghost"
                size="icon"
                onClick={() => removeMcpServer(server.name)}
              >
                <Trash2 className="h-4 w-4" />
              </Button>
            </div>
          </div>
        ))}
      </div>
    </section>
  );
}
