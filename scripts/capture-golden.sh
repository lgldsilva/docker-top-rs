#!/usr/bin/env bash
# Captura o golden do Python: `docker-top --once` normalizado.
# Máscaras: cpu/pids/timestamp (voláteis); preserva schema (colunas, ordem, unidades, exit codes, edges).
set -euo pipefail
OUT="$(dirname "$0")/../docs/golden-python-once.txt"
mkdir -p "$(dirname "$OUT")"
RAW="$(mktemp)"
/home/lgldsilva/bin/docker-top --once >"$RAW" 2>&1; EXIT_CODE=$? || true
{
  echo "# golden docker-top (Python) --once — normalizado"
  echo "# gerado: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "# cmd: docker-top --once"
  echo "# exit=$EXIT_CODE"
  echo "# mascaras: HH:MM:SS-><CLOCK>, %cpu-><CPU>, pids-><PIDS>, ids->prefixo12, bytes preservados p/ schema"
  echo "# edges: sem limite => '(H)'; contexto down => linha 'Erro de Coleta' sem crash (exit 0)"
  echo ""
  sed -E \
    -e 's/[0-9]{2}:[0-9]{2}:[0-9]{2}/<CLOCK>/g' \
    -e 's/[0-9]+\.[0-9]+%/<CPU>/g' \
    -e 's/\b[0-9]{1,3}T\b/<N>T/g' \
    "$RAW"
} > "$OUT"
echo "golden salvo em $OUT (exit=$EXIT_CODE, linhas=$(wc -l < "$OUT"))"
