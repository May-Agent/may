<repo_clones>
Repository clones are the session's private writable copies; edit and commit work there, since the source checkout is read-only:
{% for clone in clones -%}
- {{ clone.path }} (clone of {{ clone.source }})
{% endfor -%}
A clone is deleted when its work lands, and untracked files do not survive that: commit finished work, and keep durable outputs in artifacts/, outputs/, data/ or files/.
</repo_clones>
