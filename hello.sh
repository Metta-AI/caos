#!/usr/bin/env bash
lang="en"

# Parse options
while getopts ":l:" opt; do
  case "$opt" in
    l) lang="$OPTARG" ;;
    \?) echo "Unknown option: -$OPTARG" >&2; exit 1 ;;
    :) echo "Option -$OPTARG requires an argument" >&2; exit 1 ;;
  esac
done
shift $((OPTIND - 1))

first="${1:-world}"
last="$2"

case "$lang" in
  en) greeting="Hello" ;;
  es) greeting="Hola" ;;
  fr) greeting="Bonjour" ;;
  de) greeting="Hallo" ;;
  it) greeting="Ciao" ;;
  pt) greeting="Olá" ;;
  ja) greeting="こんにちは" ;;
  zh) greeting="你好" ;;
  *)
    echo "Unknown language: '$lang'" >&2
    echo "Supported: en, es, fr, de, it, pt, ja, zh" >&2
    exit 1
    ;;
esac

if [ -n "$last" ]; then
  echo "${greeting}, ${first} ${last}!"
else
  echo "${greeting}, ${first}!"
fi
