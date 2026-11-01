/** Display label for a repo binding: `owner/repo` for github: specs, the
 * path basename for local: keys. */
export function repoLabel(repo: string): string {
  if (repo.startsWith('github:')) {
    return repo.slice(7);
  }
  const rest = repo.startsWith('local:') ? repo.slice(6) : repo;
  return rest.split('/').filter(Boolean).pop() ?? rest;
}
