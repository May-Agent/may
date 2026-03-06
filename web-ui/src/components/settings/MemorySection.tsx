import { useEffect, useState } from 'react';
import { BookOpen } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Label } from '@/components/ui/label';
import { Textarea } from '@/components/ui/textarea';
import { useStore } from '@/lib/store';

export function MemorySection() {
  const { memoryContent, refreshMemory, saveMemory } = useStore();
  const [memoryOverride, setMemoryOverride] = useState('');
  const [memoryDirty, setMemoryDirty] = useState(false);
  const memoryDraft = memoryDirty ? memoryOverride : memoryContent;

  useEffect(() => {
    void refreshMemory();
  }, [refreshMemory]);

  return (
    <section className="space-y-4">
      <div className="flex items-center gap-2 text-sm font-medium uppercase text-muted-foreground tracking-wide">
        <BookOpen className="h-4 w-4" /> Memory
      </div>
      <div className="space-y-2">
        <Label>MEMORY.md</Label>
        <Textarea
          value={memoryDraft}
          onChange={(e) => {
            setMemoryOverride(e.target.value);
            setMemoryDirty(true);
          }}
          placeholder="Write memory context for future sessions..."
          className="min-h-[220px] font-mono text-sm"
        />
        <div className="flex gap-2">
          <Button
            variant="outline"
            onClick={async () => {
              await refreshMemory();
              setMemoryDirty(false);
            }}
          >
            Reload
          </Button>
          <Button
            onClick={async () => {
              await saveMemory(memoryDraft);
              setMemoryDirty(false);
            }}
            disabled={!memoryDirty}
          >
            Save memory
          </Button>
        </div>
      </div>
    </section>
  );
}
