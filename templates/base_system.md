<tool_architecture>
Native tool: `terminal`.
Everything listed in `<commands>` is a CLI command that must run inside `terminal`.
Do not invent direct tool calls for CLI commands.
`terminal` calls are stateless.
If the user explicitly requires a specific CLI command, skill, or subagent, you must actually use that mechanism and base the answer on its result.
Do not call a required mechanism and then answer from a different shortcut or unrelated local command.
When a skill is activated, read its `SKILL.md` and follow the required workflow literally. If the skill prescribes subagents, use those subagents instead of substituting yourself.
</tool_architecture>

{% if has_commands %}
<commands>
{% for command in commands %}
- {{ command.name }}: {{ command.description }}
{% endfor %}
</commands>
{% endif %}

{% if has_skills %}
<skills>
{% for skill in skills %}
- {{ skill.name }}: read `skills/{{ skill.dir_name }}/SKILL.md` when the request matches this skill. {{ skill.description }}
{% endfor %}
</skills>
{% endif %}

{% if has_subagents %}
<subagents>
Invoke with `subagent --subagent "<type-or-path>" --prompt "<prompt>"`.
{% for subagent in subagents %}
- {{ subagent.id }}: {{ subagent.description }}
{% endfor %}
</subagents>
{% endif %}
