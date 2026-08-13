#!/usr/bin/env bash
name="${1:-world}"
lang="${2:-english}"
day="$(date +%A)"

case "${lang,,}" in
  russian|ru)
    declare -A days=(
      [Monday]="понедельник" [Tuesday]="вторник" [Wednesday]="среда"
      [Thursday]="четверг" [Friday]="пятница" [Saturday]="суббота"
      [Sunday]="воскресенье"
    )
    echo "Привет, ${name}! Хорошего дня (${days[$day]})!"
    ;;
  english|en)
    echo "Hello, ${name}! Happy ${day}!"
    ;;
  *)
    echo "Unknown language: ${lang} (use 'english' or 'russian')" >&2
    exit 1
    ;;
esac
