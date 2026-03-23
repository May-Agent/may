# Slidev Themes Reference

## Using Themes

Set theme in headmatter:

```yaml
---
theme: seriph
---
```

Install theme:

```bash
bun add @slidev/theme-<name>
```

## Official Themes

### default

Built-in minimal theme. No installation required.

### seriph

Modern, clean design with serif typography. Good for most presentations.

```bash
bun add @slidev/theme-seriph
```

## Community Themes

### apple-basic

Apple Keynote-inspired design.

```bash
bun add slidev-theme-apple-basic
```

### dracula

Dark theme with Dracula color scheme.

```bash
bun add slidev-theme-dracula
```

### geist

Vercel/Geist design system.

```bash
bun add slidev-theme-geist
```

### penguin

Playful theme with illustrations.

```bash
bun add slidev-theme-penguin
```

### bricks

Clean, professional slides.

```bash
bun add slidev-theme-bricks
```

### unicorn

Colorful gradient theme.

```bash
bun add slidev-theme-unicorn
```

### shibainu

Cute theme with shiba inu illustrations.

```bash
bun add slidev-theme-shibainu
```

### academic

Academic/research presentation style.

```bash
bun add slidev-theme-academic
```

## Theme Recommendations

| Use Case | Recommended Theme |
|----------|-------------------|
| Tech talks | `seriph`, `geist` |
| Corporate | `default`, `bricks` |
| Academic | `academic`, `default` |
| Dark mode | `dracula` |
| Playful | `penguin`, `unicorn` |
| Apple-style | `apple-basic` |

## Custom Styling

Override theme styles with scoped CSS:

```markdown
<style>
h1 {
  background: linear-gradient(45deg, #4e8cff, #34d399);
  -webkit-background-clip: text;
  -webkit-text-fill-color: transparent;
}
</style>
```

Or UnoCSS classes:

```markdown
# Title {.bg-gradient-to-r .from-blue-500 .to-green-400}
```
