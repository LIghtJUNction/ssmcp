#!/system/bin/sh
# Forward one typed terminal request to the owner-only loopback listener.
set -f
umask 077
export LC_ALL=C PATH=/system/bin:/system/xbin TMPDIR=/data/adb/ssmcp
[ "$#" = 1 ] || exit 2
encoded=$1
length=${#encoded}
[ "$length" -gt 0 ] && [ "$length" -le 32768 ] && [ $((length % 4)) -eq 0 ] || exit 2
data=${encoded%%=*}
case "$data" in ''|*[!A-Za-z0-9+/]*) exit 2 ;; esac
case "${encoded#"$data"}" in
    '') padding=0 ;; '=') padding=1 ;; '==') padding=2 ;; *) exit 2 ;;
esac

MODDIR=/data/adb/modules/ssmcp
. "$MODDIR/launch.sh" || exit 3
read_console_url || exit 3
endpoint=http://127.0.0.1:42387
case "$url" in "$endpoint/#"*) token=${url##*#} ;; *) exit 3 ;; esac
[ "${#token}" = 64 ] || exit 3
case "$token" in *[!0-9a-f]*) exit 3 ;; esac

if [ -x /system/bin/curl ]; then
    # Send the private header through a descriptor, never in a process argument.
    reply=$(printf '%s' "$encoded" | base64 -d | /system/bin/curl \
        --silent --show-error --noproxy '*' --connect-timeout 3 --max-time 8 \
        --header @/proc/self/fd/3 --header 'Content-Type: application/json' \
        --data-binary @- --write-out '\n%{http_code}' "$endpoint/terminal" 3<<AUTH
Authorization: Bearer $token
AUTH
    ) || exit 4
    newline='
'
    status=${reply##*"$newline"}
    body=${reply%"$newline"*}
else
    # Keep stdin open until the HTTP server sends and closes its response.
    dir=$(mktemp -d "$TMPDIR/webui.XXXXXX") || exit 4
    trap 'rm -rf "$dir"' EXIT
    mkfifo "$dir/request" || exit 4
    timeout 8 nc -w 5 127.0.0.1 42387 <"$dir/request" >"$dir/response" &
    reader=$!
    exec 3>"$dir/request"
    cr=$(printf '\r')
    size=$((length / 4 * 3 - padding))
    cat >&3 <<HEADERS
POST /terminal HTTP/1.0$cr
Host: 127.0.0.1:42387$cr
Authorization: Bearer $token$cr
Content-Type: application/json$cr
Content-Length: $size$cr
Connection: close$cr
$cr
HEADERS
    printf '%s' "$encoded" | base64 -d >&3 || exit 4
    wait "$reader" || exit 4
    exec 3>&-
    # This listener sends compact JSON. Keep response headers out of the WebView.
    status=$(sed -n '1s:^HTTP/1\.[01] \([0-9][0-9][0-9]\).*:\1:p' "$dir/response")
    body=$(sed "1,/^${cr}\$/d" "$dir/response")
fi
case "$status" in [1-5][0-9][0-9]) ;; *) exit 5 ;; esac
case "$body" in ''|*"$token"*) exit 5 ;; esac
cat <<REPLY
$status
$body
REPLY
