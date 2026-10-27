# CI no fork Databanx

Este fork **não roda o CI do upstream**. Os workflows `ci.yml`, `scorecard.yml`,
`trivy.yml`, `auto-close-issues.yml`, `auto-close-prs.yml` e
`auto-redirect-discussions.yml` são removidos em cada branch `update/vX.Y.Z`,
de propósito.

## Por quê

`ci.yml` dispara em `push: tags: ["v*.*.*"]`, padrão que casa com as nossas tags
`vX.Y.Z-databanx-N`. Cada push de tag disparava a matriz de release completa:

| Job                       | Tempo  | Multiplicador | Minutos faturados |
| ------------------------- | ------ | ------------- | ----------------- |
| `aarch64-apple-darwin`    | 84 min | 10x           | 840               |
| `x86_64-apple-darwin`     | 61 min | 10x           | 610               |
| `x86_64-pc-windows-msvc`  | 72 min | 2x            | 144               |
| `x86_64-unknown-freebsd`  | 53 min | 1x            | 53                |
| 8 jobs Linux              | 0-2 min (todos falhando) | 1x | ~5      |

Cerca de **1.650 minutos faturados por push de tag** — quase a cota mensal
inteira, gasta em plataformas que não usamos. O binário de produção sai do
build ARM64 local em Docker (veja a skill `build-stalwart`), não do CI.

`scorecard.yml` e `trivy.yml` rodam em `schedule` (cron diário/semanal) e
existem para a postura de supply-chain do projeto upstream, não do fork.

Os três `auto-*` são gestão de comunidade do upstream. `auto-close-prs.yml` é
ativamente perigoso aqui: ele fecha **e tranca** qualquer PR cujo autor não
esteja em `.github/allowed-pr-authors.txt` — o que inclui todos os nossos.

## O que sobra

`test.yml`, que é `workflow_dispatch` puro (só roda se alguém apertar o botão)
e usa `ubuntu-latest` — custo zero em repouso.

Além disso, o Actions está desabilitado no nível do repositório
(`gh api repos/Databanx/stalwart/actions/permissions` -> `enabled: false`),
desde 2026-07-16. A remoção destes arquivos é a segunda camada: se alguém
reabilitar o Actions, nada caro volta a disparar sozinho.

## Se um dia quisermos CI de verdade aqui

Use runners Blacksmith (`runs-on: blacksmith-4vcpu-ubuntu-2404`), como nos
outros repos da Databanx (`localmail-webmail`, `localmail-gerencia`,
`localmail-admin`, `localmail-ticket`, `conectsat-platform`,
`databanx-control`) — faturado pela Blacksmith, não pelo GitHub. Só Linux;
não replique os jobs macOS/Windows, que são exatamente a parte cara e a que
não nos serve.
