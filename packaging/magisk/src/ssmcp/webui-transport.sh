#!/system/bin/sh
# The manager supplies one base64 JSON argument. The listener owns HTTP authorization.
set -f
umask 077
export LC_ALL=C PATH=/system/bin:/system/xbin TMPDIR=/data/adb/ssmcp
[ "$#" -eq 1 ] || exit 2
encoded=$1
length=${#encoded}
[ "$length" -gt 0 ] && [ "$length" -le 32768 ] && [ $((length % 4)) -eq 0 ] || exit 2
data=${encoded%%=*}
case "$data" in ''|*[!A-Za-z0-9+/]*) exit 2 ;; esac
case "${encoded#"$data"}" in
    '') padding=0 ;;
    '=') padding=1 ;;
    '==') padding=2 ;;
    *) exit 2 ;;
esac
size=$((length / 4 * 3 - padding))

MODDIR=/data/adb/modules/ssmcp
. "$MODDIR/launch.sh" || exit 3
read_console_url || exit 3
prefix='http://127.0.0.1:42387/#'
case "$url" in "$prefix"*) token=${url#"$prefix"} ;; *) exit 3 ;; esac
[ "${#token}" -eq 64 ] || exit 3
case "$token" in *[!0-9a-f]*) exit 3 ;; esac

# Keep the bearer in the pipe, out of child command lines and manager callbacks.
cr=$(printf '\r')
reply=$(
    {
        cat <<HEADERS
POST /terminal HTTP/1.0$cr
Host: 127.0.0.1:42387$cr
Authorization: Bearer $token$cr
Content-Type: application/json$cr
Content-Length: $size$cr
Connection: close$cr
$cr
HEADERS
        printf '%s' "$encoded" | base64 -d
    } | timeout 8 nc -w 5 127.0.0.1 42387 || exit 4
    # Preserve trailing newlines until Content-Length has been checked.
    printf '.'
) || exit 4
reply=${reply%.}

crlf=$(printf '\r\n.')
crlf=${crlf%.}
separator=$crlf$crlf
case "$reply" in *"$separator"*) ;; *) exit 5 ;; esac
headers=${reply%%"$separator"*}
body=${reply#*"$separator"}
line=${headers%%"$crlf"*}
case "$line" in
    HTTP/1.[01]' '[1-5][0-9][0-9]|HTTP/1.[01]' '[1-5][0-9][0-9]' '*) ;;
    *) exit 5 ;;
esac
status=${line#* }
status=${status%% *}
# The daemon returns compact JSON with a fixed Content-Length.
content_length=
while [ "$headers" != "$line" ]; do
    headers=${headers#*"$crlf"}
    line=${headers%%"$crlf"*}
    case "$line" in
        [Cc][Oo][Nn][Tt][Ee][Nn][Tt]-[Ll][Ee][Nn][Gg][Tt][Hh]': '*)
            [ -z "$content_length" ] || exit 5
            content_length=${line#*: }
            case "$content_length" in ''|*[!0-9]*) exit 5 ;; esac ;;
        [Tt][Rr][Aa][Nn][Ss][Ff][Ee][Rr]-[Ee][Nn][Cc][Oo][Dd][Ii][Nn][Gg]:*) exit 5 ;;
    esac
done
bytes_with_newline=$(wc -c <<BODY
$body
BODY
) || exit 5
[ "$((bytes_with_newline - 1))" = "$content_length" ] || exit 5
case "$body" in *"$token"*) exit 5 ;; esac
cat <<REPLY
$status
$body
REPLY
