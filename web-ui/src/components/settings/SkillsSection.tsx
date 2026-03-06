import { useMemo, useState } from 'react';
import { Wrench, Plus, Trash2 } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { useStore } from '@/lib/store';

export function SkillsSection() {
  const { skills, installSkill, deleteSkill } = useStore();
  const [skillRepo, setSkillRepo] = useState('');

  const sortedSkills = useMemo(
    () => [...skills].sort((a, b) => a.name.localeCompare(b.name)),
    [skills],
  );

  return (
    <section className="space-y-4">
      <div className="flex items-center gap-2 text-sm font-medium uppercase text-muted-foreground tracking-wide">
        <Wrench className="h-4 w-4" /> Skills
      </div>
      <div className="flex gap-2">
        <Input value={skillRepo} onChange={(e) => setSkillRepo(e.target.value)} placeholder="owner/repo" />
        <Button
          onClick={async () => {
            if (!skillRepo.trim()) return;
            await installSkill(skillRepo.trim());
            setSkillRepo('');
          }}
        >
          <Plus className="h-4 w-4 mr-2" /> Install
        </Button>
      </div>
      <div className="space-y-2">
        {sortedSkills.map((skill) => (
          <div key={skill.dirName} className="flex items-center justify-between rounded border px-3 py-2">
            <div>
              <div className="font-medium">{skill.name}</div>
              <div className="text-xs text-muted-foreground">{skill.description}</div>
              <div className="text-xs text-muted-foreground">{skill.source}</div>
            </div>
            <Button variant="ghost" size="icon" onClick={() => deleteSkill(skill.dirName)}>
              <Trash2 className="h-4 w-4" />
            </Button>
          </div>
        ))}
      </div>
    </section>
  );
}
