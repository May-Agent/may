<repo_clones>
Repository workspaces are private writable clones derived on demand; each holds one change and one PR (derive another with `repo clone --new` for concurrent PRs):
{% for clone in clones -%}
- {{ clone.path }} (clone of {{ clone.source }})
{% endfor -%}
A workspace is deleted when its PR merges or closes, and untracked files do not survive that: commit finished work, and keep durable outputs in artifacts/, outputs/, data/ or files/.
</repo_clones>
