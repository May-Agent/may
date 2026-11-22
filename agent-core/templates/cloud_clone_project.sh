set -eu
if [ -d {{ directory }}/.git ]; then exit 0; fi
IFS= read -r token
auth=$(printf 'x-access-token:%s' "$token" | base64 | tr -d '\n')
git -c "http.https://github.com/.extraheader=AUTHORIZATION: basic $auth" clone {{ url }} {{ directory }}
