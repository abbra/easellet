#!/bin/sh
# Every value printed below is a number or a fixed word, so no JSON escaping is
# needed; a plugin that prints free text should use a language with a JSON encoder.
cat > /dev/null    # the input document; this plugin takes no parameters

load=$(cut -d' ' -f1 /proc/loadavg)
cores=$(getconf _NPROCESSORS_ONLN)
mem_total=$(awk '/MemTotal/ {print $2}' /proc/meminfo)
mem_free=$(awk '/MemAvailable/ {print $2}' /proc/meminfo)
mem_used=$(( (mem_total - mem_free) * 100 / mem_total ))
disk_used=$(df -P / | awk 'NR == 2 {sub("%", "", $5); print $5}')
up_days=$(awk '{print int($1 / 86400)}' /proc/uptime)

status() { # value warn fail
    if [ "$1" -ge "$3" ]; then echo fail; elif [ "$1" -ge "$2" ]; then echo warn; else echo ok; fi
}
load_pct=$(awk -v load="$load" -v cores="$cores" 'BEGIN {print int(load * 100 / cores)}')

printf '{"api":1,"output":{"type":"card","layout":"list","title":"Мост: состояние","accent":"blue","items":['
printf '{"text":"Нагрузка %s на %s ядер","status":"%s"},' "$load" "$cores" "$(status "$load_pct" 70 100)"
printf '{"text":"Память занята на %s%%","status":"%s"},' "$mem_used" "$(status "$mem_used" 80 95)"
printf '{"text":"Диск / занят на %s%%","status":"%s"},' "$disk_used" "$(status "$disk_used" 85 95)"
printf '{"text":"Работает %s дн. без перезагрузки","status":"info"}' "$up_days"
printf ']}}\n'
