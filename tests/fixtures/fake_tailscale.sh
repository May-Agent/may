#!/bin/sh
# A stand-in for the `tailscale` CLI. It logs every call to calls.log next to
# itself and keeps the Funnel configuration in serve.json, so the daemon's
# reconcile-to-spec logic can be driven without a real Tailscale.
dir=$(dirname "$0")
echo "$*" >> "$dir/calls.log"

case "$1" in
status)
  echo '{"BackendState":"Running","Self":{"DNSName":"mac.tail1234.ts.net.","UserID":1},"User":{"1":{"LoginName":"me@example.com"}}}'
  ;;
serve)
  if [ "$2" = status ]; then
    cat "$dir/serve.json" 2>/dev/null || echo '{}'
  fi
  ;;
funnel)
  port=443
  path=/
  for arg in "$@"; do
    case "$arg" in
    --https=*) port=${arg#--https=} ;;
    --set-path=*) path=${arg#--set-path=} ;;
    esac
    last=$arg
  done
  if [ "$last" = off ]; then
    echo '{}' > "$dir/serve.json"
  else
    host="mac.tail1234.ts.net:$port"
    printf '{"TCP":{"%s":{"HTTPS":true}},"Web":{"%s":{"Handlers":{"%s":{"Proxy":"%s"}}}},"AllowFunnel":{"%s":true}}\n' \
      "$port" "$host" "$path" "$last" "$host" > "$dir/serve.json"
  fi
  ;;
esac
