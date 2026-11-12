/** Shown when the remote listener answers 401. There is no sign-in form:
 * remote access is authenticated only by the visitor's Tailscale identity. */
export function SignInRequired() {
  return (
    <div className="flex h-full min-h-screen flex-col items-center justify-center gap-3 p-8 text-center">
      <h1 className="text-xl font-semibold">Tailnet access only</h1>
      <p className="max-w-md text-sm text-muted-foreground">
        This May instance is reachable remotely only through Tailscale, by a
        tailnet login on its allow-list. There is no sign-in form: open it from
        a device signed in to an allowed tailnet login, or ask the owner to add
        yours in settings.
      </p>
    </div>
  );
}
