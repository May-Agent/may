/** Placeholder for the remote listener's auth wall. Passkey sign-in (#18)
 * replaces this screen; until then the app cannot proceed without a trusted
 * identity header. */
export function SignInRequired() {
  return (
    <div className="flex h-full min-h-screen flex-col items-center justify-center gap-3 p-8 text-center">
      <h1 className="text-xl font-semibold">Sign in required</h1>
      <p className="max-w-md text-sm text-muted-foreground">
        This May instance requires an identity the browser did not provide.
        Sign-in is coming; reach the daemon on its local address or through
        Tailscale until then.
      </p>
    </div>
  );
}
