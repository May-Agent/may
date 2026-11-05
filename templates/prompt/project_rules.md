<project_rules>
{% if let Some(source) = truncated_root %}The launch directory's AGENTS.md below is cut to fit the context; read {{ source }} for the rest.
{% endif %}{% if !nested.is_empty() %}These AGENTS.md files under the launch directory govern their own subtrees and are not loaded; before working under one's directory, read it:
{% for path in nested %}- {{ path }}
{% endfor %}{% if extra_nested > 0 %}- … and {{ extra_nested }} more
{% endif %}{% endif %}When you write a compaction handoff, link every AGENTS.md the next turns still need; the files it links are loaded for you.
</project_rules>
