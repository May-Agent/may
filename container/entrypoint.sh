#!/bin/bash
# Resolve hostname to IP for Chrome CDP compatibility (Chrome rejects non-IP Host headers).
if [ -n "$BROWSER_CDP_ENDPOINT" ]; then
  HOST_IP=$(getent hosts host.docker.internal 2>/dev/null | awk '{print $1}')
  if [ -n "$HOST_IP" ]; then
    BROWSER_CDP_ENDPOINT=$(echo "$BROWSER_CDP_ENDPOINT" | sed "s|host.docker.internal|$HOST_IP|g")
    export BROWSER_CDP_ENDPOINT
  fi
  printf '{"browser":{"cdpEndpoint":"%s"}}\n' "$BROWSER_CDP_ENDPOINT" \
    > /root/playwright-cli.json
  # Wrapper so playwright-cli always uses the CDP config
  cat > /usr/local/bin/pw <<'WRAPPER'
#!/bin/bash
exec playwright-cli --config /root/playwright-cli.json "$@"
WRAPPER
  chmod +x /usr/local/bin/pw
fi
exec "$@"
