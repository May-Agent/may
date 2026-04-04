---
name: slide
description: "MUST use for slide/presentation requests. Create Markdown-based Slidev decks. Trigger on slides, presentation, deck, PPT, slide deck, presentation file, briefing deck."
---

# Slide Skill (Slidev)

## Pre-flight Check

Ask: **"Will you edit this in Keynote/PowerPoint later?"**
- YES: Stop. Recommend using those apps directly.
- NO: Continue.
- If the user already clearly says they will not edit in Keynote/PowerPoint later, skip the question and continue.

## Workflow

### Phase 1: Research (Optional)

If the topic needs research, use the builtin research subagent:

```bash
subagent --subagent "research" --prompt "Research [TOPIC] for a presentation. Find key concepts, recent statistics, and expert insights."
```

If the user asks for current, recent, or externally grounded material, research is required.

### Phase 2: Requirements (Main Agent)

Gather from user:
- Topic and key message
- Audience: technical / general / executive
- Slide count (default: 5-8)
- Style: dark / light / minimal / corporate / playful

### Phase 3: Art Direction (Subagent)

Launch art direction subagent to create design guide and outline:

```bash
subagent --subagent "skills/slide/subagents/art_direction.md" --prompt "Create design guide for: [TOPIC]. Audience: [AUDIENCE]. Style: [STYLE]. Slide count: [N]."
```

The subagent returns a YAML design guide containing:
- Theme selection
- Color palette
- Typography guidance
- Slide-by-slide outline with layouts and content

Even if the user currently wants only an outline or design review, this phase is still required. Do not skip the art direction subagent and do not substitute the main agent's own outline.

### Phase 4: User Approval

Present the outline to the user:
- Show slide titles and structure
- Confirm design direction
- **Get approval before proceeding**

If the user asked only for outline/design review, stop after this phase.

### Phase 5: Parallel Slide Creation (Subagents)

Launch one slide_creator subagent per slide **in parallel**:

```bash
subagent --subagent "skills/slide/subagents/slide_creator.md" --prompt "Slide 1: [SPEC]. Design: [DESIGN_GUIDE]"
subagent --subagent "skills/slide/subagents/slide_creator.md" --prompt "Slide 2: [SPEC]. Design: [DESIGN_GUIDE]"
subagent --subagent "skills/slide/subagents/slide_creator.md" --prompt "Slide 3: [SPEC]. Design: [DESIGN_GUIDE]"
# ... one per slide
```

Each subagent:
1. Creates standalone temp file with slide content
2. Renders to PNG for visual verification
3. Views the image to ensure quality
4. Iterates until the slide looks correct
5. Returns verified slide markdown

### Phase 6: Assembly & Preview (Main Agent)

Collect all slide outputs and assemble into final presentation:

```bash
mkdir -p artifacts
cat > artifacts/slides.md << 'EOF'
---
theme: [from design guide]
title: [presentation title]
transition: [from design guide]
---

[Slide 1 content]

---

[Slide 2 content]

---

[... all slides ...]
EOF
```

Clean up temp files:

```bash
rm -rf artifacts/temp
```

Then return artifact tag for frontend-native Slidev rendering:

```xml
<artifact url="slides.md" slidev />
```

Strict output contract:
- Final response MUST include `<artifact url="slides.md" slidev />` (with `slidev` attribute).
- Do NOT output localhost preview URLs (for example `http://localhost:3030`).
- Use Slidev markdown + artifact tag only.
- Do not replace this with `<artifact url="slides.md" />` (missing `slidev` is invalid for this skill).

Do not start a local preview server for normal preview flow. The frontend renders Slidev from the markdown artifact directly.

### Phase 7: Export (Optional)

If user wants PDF:

```bash
cd artifacts && bunx @slidev/cli export slides.md --output slides.pdf
```

## Reference Files

Read these from the skill directory:

- `skills/slide/references/syntax.md` - Full Slidev syntax guide
- `skills/slide/references/layouts.md` - Layout options and usage
- `skills/slide/references/themes.md` - Theme gallery and recommendations
