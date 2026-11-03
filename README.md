# docker-top-rs

Monitor interativo de recursos de containers Docker — port Rust do `docker-top` (Python/`rich`).
Binário: `docker-top-rs` (não sombreia o Python `docker-top`).

## Origem

Port fiel de `/home/lgldsilva/bin/docker-top` (1248 linhas): mesmos parsers, mesmas
16 colunas de sort, mesmos 3 layouts responsivos, mesmo cálculo de limites
CPU/Mem relativos, cache de `inspect`, `Tab` por contexto, `--once`.

## Requisitos P0 (review kimi k3)

- **Dispatch de transporte por contexto, 1x no startup**: lê
  `~/.docker/contexts/meta/*/meta.json` + `docker context ls`.
  `unix://` → **Bollard** (rustls, sem openssl); `ssh://` (homeserver,
  oci-ampere-1/2) → `tokio::process docker --context … stats/inspect`.
  Decisão fixada no startup (sem flapping por request), badge `api/cli` na TUI,
  uma linha de log por contexto no stderr.
- **Config TOML dinâmica** `~/.config/docker-top/config.toml`
  (hierarquia `default < arquivo < env DOCKER_TOP_* < CLI`, `figment` + `serde`
  `deny_unknown_fields`). Zero hardcode: `refresh_delay`, sort default, timeouts
  (`context_ls` 3s, `info` 3s, `stats` 30s — hosts ssh com dezenas de containers
  medem ~8s só de `stats`, `inspect` 30s), thresholds cor
  50/85, larguras 100/160, formatos, keybindings, colunas. Hot reload tecla `R`
  + `SIGHUP`. `config.example.toml` versionado (`--generate-config-example`).
- **Trait `DockerProvider`** (`Bollard` / `Cli` / `Mock`) p/ testabilidade.
  Cache `HashMap<(ctx,id), Limits>` com evict quando o id some; cache
  `SystemInfo` por contexto.
- **Parsers sem regex pesada** + `proptest` + unit tests.
- **Render `ratatui` + `crossterm`**, tick de coleta != render (event-driven,
  sem 10fps fixo), 3 layouts responsivos, sort 16 colunas, filtro, `Tab` troca
  contexto, `--once`.
- **F0**: `docs/golden-python-once.txt` (Python `--once` normalizado: máscara
  cpu/pids/timestamp; schema: colunas, ordem, unidades, exit codes, edges sem
  limite / contexto down).
- `Cargo.toml`: `tokio` full, `ratatui`, `crossterm`, `clap` derive, `serde`,
  `figment`, `bollard` rustls, `anyhow`. `LTO + strip` no release, bin
  `docker-top-rs`.
- README, snapshot `insta` (`TestBackend`), `cargo clippy/fmt/test` verde.

## Uso

```bash
docker-top-rs                  # TUI (todos os contextos)
docker-top-rs --once           # uma leitura, TSV p/ stdout (erros no fim)
docker-top-rs -c homeserver -s cpu_rel
docker-top-rs --log /tmp/dt.log          # log por contexto/tick (duração, erros)
docker-top-rs --ssh-direct               # ssh:// via `ssh <host> docker ...` direto
docker-top-rs --ssh-hosts                # + hosts de ~/.ssh/config com daemon docker
docker-top-rs --generate-config-example
```

Teclas (todas remapeáveis em `[keybindings]`): `x` cpu-rel, `m` mem-rel,
`c/u/l/k` físicos/limites, `1/2/r` rede, `3/4/d` disco, `p` pids, `n` nome,
`i` id, `s` ordem, `Tab`/`o` contexto, `←/→` coluna, `↑/↓` ordem, `f` filtro,
`R` reload config, `+/-` refresh, `q` sair.

## Troubleshooting remotos (ssh://)

- **Contexto remoto some/sem containers**: o badge fica `nome:cli!` no header e
  a linha vermelha no footer mostra o erro do último tick (`⚠ ctx: msg`).
  No `--once` os erros aparecem na seção `errors:`. Sem erro e ainda vazio?
  Use `--log` e confira a duração/linhas por tick.
- **Primeiro tick demora**: cada host ssh paga handshake + `stats --no-stream`
  (hosts com ~60 containers: ~8s). As fases de coleta e enriquecimento rodam
  em paralelo por contexto; o `info` tem cache de 60s por contexto.
- **`stats` estourando timeout**: suba `[timeouts] stats` no config (default 30s)
  ou `DOCKER_TOP_TIMEOUTS__STATS=30`.
- **SSH mais rápido**: `ControlMaster auto` + `ControlPersist` no
  `~/.ssh/config` reusa a conexão entre ticks (a ferramenta faz spawn por tick).
- **`--ssh-direct`**: roda `ssh -o BatchMode=yes -o ConnectTimeout=5
  -o StrictHostKeyChecking=accept-new <host> docker stats …`, sem passar pelo
  mecanismo de docker context (o `--context` local não atravessa o ssh).
  Útil quando o context falha ou p/ controlar as opções ssh.
- **`--ssh-hosts`**: lê aliases de `~/.ssh/config` (sem globs), sonda
  `ssh <host> docker version` em paralelo e adiciona os que têm daemon como
  contextos extras — mesmo sem docker context definido. Esses contextos só
  funcionam via SSH direto (são marcados automaticamente).

## Config

```bash
cp config.example.toml ~/.config/docker-top/config.toml
DOCKER_TOP_REFRESH_DELAY=1.5 docker-top-rs
DOCKER_TOP_TIMEOUTS__STATS=10 docker-top-rs
```
