# Grupos e listas no Stalwart v0.16.26 (fork Databanx)

Referência para quem integra com o Stalwart que a Databanx roda: o fork em `Databanx/stalwart`, merge #23 (`5d6f3b3e7`), versão **v0.16.26**, a mesma de produção. Trata de **grupos** (`x:Account` com `@type: "Group"`) e de **listas** (`x:MailingList`).

A página oficial (<https://stalw.art/docs/auth/principals/group/>) descreve um modelo anterior à 0.16. Fala, por exemplo, de um campo `members` no grupo e de grupos aninhados, e nenhum dos dois existe no código da 0.16.26. A comparação completa está na seção 14.

**Como ler.** Cada regra cita `caminho:linha` relativo à raiz do repositório. A seção 13 junta o que foi **provado em servidor** (dev01, v0.16.26, 2026-10-23). No resto do texto, **[código]** marca o que foi lido no código, **[provado]** o que também foi visto no servidor do dev01 e **[produção, 2026-10-23]** o que foi consultado (só leitura) na configuração de produção. Quando algo não existe no código, o texto diz "não encontrado no código".

---

## 1. Resumo para quem integra

- **Grupo é uma conta** (`Account::Group`) com caixa própria, quota própria e endereço próprio. Não faz login: não tem `credentials`, o login por diretório recusa conta que não é User (`crates/common/src/cache/directory.rs:76-86`) e o token do grupo não tem a permissão `authenticate` (`crates/common/src/auth/authentication.rs:44`). **[código]** `crates/registry/src/schema/structs.rs:12-15`, `:2964-2990`
- **Quem é membro fica gravado no User**, em `memberGroupIds`. O grupo não tem `members` nem `memberOf`, e **não há aninhamento** de grupos. **[código]** `structs.rs:6237-6238`, `crates/common/src/cache/principals.rs:563`
- **Diretório externo pode reescrever os membros:** se o diretório devolve grupos (no OIDC, com `claimGroups` configurado), cada login substitui o `memberGroupIds` do User. A Gerência só é dona da associação com `claimGroups` vazio, que é o caso no dev01 e em produção **[produção, 2026-10-23]**. Ver seção 3.
- **`memberTenantId` é chave estrangeira com conferência de tenant.** Um objeto com tenant A só pode apontar para Domain, Group (via `memberGroupIds`) e Role que também sejam do tenant A. Um objeto **sem** tenant não passa por essa conferência. No update, só as chaves novas são conferidas. **[código + provado]** `crates/store/src/registry/write.rs:132`, `:178-244`
- **O grupo precisa de `emailReceive` no próprio token.** Em produção, `defaultGroupRoleIds` é vazio e não existe a role "Group" **[produção, 2026-10-23]**. Sem permissão explícita, o SMTP responde `250` e depois devolve bounce `550 This account is not authorized to receive` **[provado]**. Receita: `roles: {"@type":"Custom","roleIds":{}}` + `permissions: {"@type":"Merge","enabledPermissions":{"emailReceive":true}}` **[provado]**. Com tenant, o `emailReceive` só sobrevive se as permissões do tenant também o tiverem (seção 5); em produção a role dos tenants ("Default User") tem **[produção, 2026-10-23]**.
- **O membro não herda permissões do grupo.** O acesso dele à caixa do grupo vem do fato de ser membro (`member_of` no token). **[código]** `crates/common/src/auth/access_token.rs:45-76`
- **Sessão JMAP do membro:** a conta pessoal (`isPersonal: true`) mais uma conta por grupo (`isPersonal: false`, `isReadOnly: false`). A conta pessoal ganha sozinha uma Identity com o endereço do grupo. **[código + provado]**
- **Enviar como o grupo** funciona pela conta pessoal (com a Identity do grupo) e pela conta do grupo. O grupo **não** precisa de `emailSend`. **[provado]**
- **Quota:** o que chega ao grupo conta no `usedDiskQuota` do grupo e do tenant dele. **[provado]**
- **Limites:** `maxGroups` (e não `maxAccounts`) só vale quando quem cria é admin de tenant. A licença Enterprise conta grupos como contas. **[código]**
- **Apagar:** um Group com membros devolve `objectIsLinked`. Tire os membros e destrua no mesmo `x:Account/set` **[provado]**. A caixa só é apagada depois, pela tarefa `DestroyAccount` **[código]**.
- **API de gerência:** no máximo 16 method calls por request (padrão de `Jmap.maxMethodCalls`), porque acima disso o request inteiro é recusado. O `update` de um `/set` tem de ser um **objeto** JSON. **[código]**

---

## 2. Group: schema e campos

Definição: `crates/registry/src/schema/structs.rs:2964-2990`. Validação, índices e patch: `crates/registry/src/schema/structs_impl.rs:21555-21617` e `:21693-21725`.

| Campo | Tipo | Situação | Fonte |
|---|---|---|---|
| `@type` | `"Group"` | Obrigatório na prática. Sem ele, o `Account` padrão é `User` (`structs_impl.rs:37-40`). | `structs.rs:11-15` |
| `name` | string | Obrigatório. É a parte local do endereço (validador `EmailLocalPart`). | `structs_impl.rs:21558-21561`, `:21700-21703` |
| `domainId` | Id | Obrigatório. Chave estrangeira para `Domain`. | `:21562-21565`, `:21600` |
| `description` | string? | Opcional. Se vier, não pode ser vazio. | `:21566-21570` |
| `createdAt` | data | `serverSet` | `:21707` |
| `memberTenantId` | Id? | Opcional. Chave estrangeira para `Tenant`. Só um admin sem tenant pode gravar. | `:21575-21579`, `:21605`, `:21708-21710` |
| `roles` | `{"@type":"Default"}` ou `{"@type":"Custom","roleIds":{…}}` | Padrão: `Default`. Em `Custom`, cada role é chave estrangeira para `Role`. | `structs.rs:4528-4532`, `structs_impl.rs:6475-6492`, `:21609` |
| `permissions` | `{"@type":"Inherit"}`, `Merge` ou `Replace` com `enabledPermissions`/`disabledPermissions` | Padrão: `Inherit`. | `structs.rs:4204-4218`, `structs_impl.rs:31105-31109` |
| `quotas` | mapa `StorageQuota → u64` (ex.: `maxDiskQuota`) | Opcional | `structs.rs:2981-2982` |
| `aliases` | lista de `EmailAlias` (`enabled`, `name`, `domainId`, `description`) | Opcional. O `domainId` de cada alias é chave estrangeira para `Domain`. | `structs.rs:2827-2836`, `structs_impl.rs:20540-20542` |
| `locale`, `timeZone`, `externalId` | — | Opcionais. `locale` padrão: `en-US`. | `structs_impl.rs:21655-21672` |
| `emailAddress` | string | `serverSet`, calculado no `get` como `name@domínio` | `:21705`; `crates/jmap/src/registry/get.rs:258-272` |
| `usedDiskQuota` | número | `serverSet`, lido do contador no `get` | `:21713`; `get.rs:249-257` |

**Rejeitados:** qualquer outro campo cai em `"Invalid property"` (`structs_impl.rs:21723`). Isso vale para `members`, `memberOf`, `memberGroupIds` (que existe só no User), `lists`, `picture`, `emails`, `quota` (no singular) e `type` (sem o `@`).

**Unicidade:** `name`+`domainId` vão para um índice único global (`unique_global_composite`, `structs_impl.rs:21597`), o mesmo usado por User, MailingList e aliases.

**Trocar o `@type`** de uma conta que já existe é recusado com `invalidProperties` e a mensagem "Cannot change the type of an existing account." (`crates/jmap/src/registry/mapping/principal.rs:244-251`).

---

## 3. Membros

- **A ligação fica no User:** `UserAccount.memberGroupIds: Map<Id>` (`structs.rs:6237-6238`). Cada id é chave estrangeira para `Account`, **com filtro de tipo `Group`** (`structs_impl.rs:47104-47110`). Apontar para um User falha com `invalidForeignKey`.
- **`Map` é um conjunto.** Para pôr: `"memberGroupIds/<idGrupo>": true`. Para tirar: `null` ou `false`. Um objeto inteiro substitui o conjunto (`crates/registry/src/types/map.rs:135-200`).
- **Sem aninhamento.** O Group não tem `memberGroupIds`, e o cache do grupo nasce com `id_member_of` vazio (`crates/common/src/cache/principals.rs:563`). O token do grupo também tem `member_of` vazio (`access_token.rs:251`).
- **Listar os membros de um grupo** exige busca reversa: `x:Account/query` com filtro em `memberGroupIds`, que é indexado como campo de busca (`structs_impl.rs:47111-47113`). O grupo não guarda a lista de membros.
- **Mudar membros muda o token.** `memberGroupIds` entra no hash da conta (`access_token.rs:865-893`), então o token do membro é refeito.
- **Diretório externo e `memberGroupIds`.** Se o diretório (OIDC, LDAP, SQL) devolve `groups`, o login **substitui** todo o `memberGroupIds` do User pela lista do diretório e cria os grupos que faltam (`crates/common/src/cache/directory.rs:118-140`, `:282`). No OIDC isso só acontece com `claimGroups` configurado (`crates/directory/src/backend/oidc/lookup.rs:240-254`). No dev01 e em produção o diretório OIDC (ZITADEL) tem `claimGroups: null`, então os membros gravados pela Gerência ficam **[produção, 2026-10-23]**. Ligar `claimGroups` muda isso.

---

## 4. Tenant como chave estrangeira

**Regra [código]** (`crates/store/src/registry/write.rs:178-244`). Em cada chave estrangeira do objeto gravado:

1. o objeto apontado precisa existir (`:197-225`);
2. **se o objeto gravado tiver `memberTenantId` = T** e o tipo apontado tiver a flag `OBJ_FILTER_TENANT`, o apontado precisa estar indexado com `memberTenantId` = T. Se não estiver, a resposta é `invalidForeignKey` (`:226-244`).

Tipos com `OBJ_FILTER_TENANT`: `Account` (`structs_impl.rs:11-12`), `Domain` (`:19840-19841`), `MailingList` (`:24419-24420`) e `Role` (`:33383-33384`), entre outros. `Tenant` não tem a flag (`:44332`).

Consequências:

| Objeto gravado | Aponta para | Resultado |
|---|---|---|
| User/Group com tenant A | Domain sem tenant | `invalidForeignKey` (Domain) **[provado]** |
| Group sem tenant | Domain do tenant A | aceito, porque não há conferência **[provado]** |
| User com tenant A | Group sem tenant ou de outro tenant | `invalidForeignKey` (Account) **[provado]** |
| User sem tenant | Group do tenant A | aceito **[provado]** |
| Group com tenant A | Role sem tenant (`roles: Custom`) | `invalidForeignKey` (Role) **[provado]** |

**Só as chaves novas são conferidas** (`write.rs:123-132`). No update, o servidor tira do conjunto a conferir as chaves que já existiam. Por isso, dar `memberTenantId` A por update a um User sem tenant **não** reconfere o Domain, porque a chave estrangeira para ele não mudou **[provado]**. No caso simétrico, trocar o tenant de um **Group** não reconfere os Users que já apontam para ele, porque a chave fica no User **[código]**. Pelo mesmo motivo, os `memberGroupIds` antigos também não seriam reconferidos. Essa segunda parte é **[código]**, não provada.

**Quem pode gravar o tenant [código]:**
- Só um token **sem** tenant pode mexer em `memberTenantId`. Os outros recebem "Cannot modify memberTenantId property" (`crates/registry/src/jmap/patch.rs:130-138`; `crates/jmap/src/registry/set.rs:116`).
- Quando um admin de tenant cria o objeto, o servidor grava o tenant do admin à força (`set.rs:378-381`).
- No update e no destroy, um admin de tenant só enxerga objetos do próprio tenant. Os outros aparecem como `notFound` (`set.rs:305-312`, `:679-684`).

---

## 5. Roles e permissões do grupo

**Como o token do grupo é montado [código]** (`access_token.rs:217-230`):
- `roles: Default` usa `Authentication.defaultGroupRoleIds` (`structs.rs:469-470`; `crates/common/src/network/security.rs:189`).
- `roles: Custom {ids}` usa exatamente esses ids. Com `{}`, o grupo fica **sem nenhuma role**, porque `role_ids()` devolve `Some(&[])` e o padrão não entra (`crates/registry/src/utils/mod.rs:20-27`).
- Em `effective_permissions` (`crates/common/src/auth/permissions.rs:44-103`):
  - `Inherit` usa só as roles;
  - `Merge` soma a lista às roles;
  - `Replace` ignora as roles (`:51-59`);
  - na Enterprise, se o grupo tem tenant, o resultado é **restringido às permissões do tenant** (`:71-98`). Um tenant com `roles: Default` usa `defaultTenantRoleIds` (`crates/common/src/cache/principals.rs:826-839`). Ou seja: o `emailReceive` dado ao grupo só vale se as permissões efetivas do tenant também incluírem `emailReceive`.

**Padrão de fábrica.** Só numa instalação nova o bootstrap cria as roles "User", "Group", "Tenant Administrator" e "System Administrator", e põe "Group" em `defaultGroupRoleIds` (`crates/common/src/manager/defaults.rs:402-458`). A role "Group" leva as permissões `jmap*`, `imap*`, `pop3*`, `calendar*`, `email*`, `dav*` e `sieve*`, entre outras (`permissions.rs:263-283`). O dev01 está assim.

**Produção é diferente [produção, 2026-10-23]:** `defaultGroupRoleIds` e `defaultTenantRoleIds` são vazios e não existe a role "Group". As roles de produção são System Administrator, Default User e o robô de migração. Os tenants têm `roles: Custom {Default User}`, e essa role inclui `emailReceive`. Um Group com `roles: Default` ou `Custom {}` e `permissions: Inherit` fica **sem permissão nenhuma**.

**Por que dar `emailReceive` explícito.** A entrega local monta o token **da conta destinatária**, ou seja, do grupo, e exige `emailReceive` (`crates/email/src/message/delivery.rs:168-173`). Se faltar, a falha é permanente: `550 5.5.0 This account is not authorized to receive email.` (`:243-248`). O SMTP já tinha aceitado o `RCPT`, porque essa etapa só confere se a conta existe (`crates/common/src/network/mta.rs:151-160`). O resultado é `250` seguido de bounce **[provado]**.

**Por que `Custom {}` e não `Default`:** com `Custom {}`, o grupo não depende de `defaultGroupRoleIds`, que difere entre dev01 e produção e pode mudar. Já `Custom` com uma role sem tenant não serve para grupo com tenant, porque a chave estrangeira falha (seção 4) **[provado]**; as roles de produção não têm tenant **[produção, 2026-10-23]**.

**Validação na gravação:** ao criar o grupo, ou ao mudar `roles`/`permissions`, o servidor confere se quem chama pode conceder aquelas permissões (`principal.rs:206-208`, `:235-243`, `:254-262`; `permissions.rs:106-140`).

**O membro não herda permissões do grupo [código].** O token do User usa só `permissions` e `roles` do próprio User (`access_token.rs:45-69`). `memberGroupIds` entra só em `member_of` e no cálculo de ACL (`:71-123`). As permissões do grupo valem apenas para o token do próprio grupo, o que na prática significa a entrega.

---

## 6. Acesso do membro e sessão JMAP

**[código]**
- `secondary_ids()` = `member_of` + contas que deram acesso ao usuário por ACL (`access_token.rs:443-448`). `is_member(id)` é verdadeiro para a própria conta, para os grupos e para quem tem `impersonate` (`:476-480`).
- A sessão JMAP (`crates/jmap/src/api/session.rs`) é montada assim:
  - a conta pessoal entra com `isPersonal: true`, `isReadOnly: false` e é a `primaryAccounts` de todas as capacidades (`:42-58`);
  - cada id secundário entra com `isPersonal: false` e **`isReadOnly: false` sempre**, mesmo quando o acesso é só leitura por ACL (`:60-93`).
- As capacidades listadas para a conta do grupo vêm das **permissões do membro** (`account_capabilities()` do token dele, `:83-90` e `:100-133`), e não das do grupo.
- A checagem por método usa o token de quem chama (`crates/jmap/src/api/request.rs:248`). Ser membro dá acesso total à conta do grupo (`has_access` → `is_member`, `access_token.rs:696-704`). `Identity/set` e `EmailSubmission/set` na conta do grupo exigem `assert_is_member` (`request.rs:485-497`).
- No IMAP, as caixas do grupo aparecem em `Shared Folders/<grupo@domínio>`, prefixo configurável em `defaultFolders` (entrada `shared`), padrão "Shared Folders" (`crates/imap/src/core/mailbox.rs:56-75`; `access_token.rs:683-694`; `crates/common/src/config/mailstore/email.rs:149`, `:162-165`). Não foi provado em servidor.

**Identities automáticas [código]** (`crates/jmap/src/identity/get.rs:151-291`):
- Os endereços de uma conta incluem os dos grupos dela (`principals.rs:714-729`).
- No `Identity/get`, as Identities que faltam são criadas (`get.rs:226-257`). A conta pessoal do membro ganha `grupo@` **[provado]**.
- O nome dessas Identities é o `description` (ou o `name`) **do membro**, e não do grupo (`get.rs:228`).
- Quando o membro sai do grupo, a Identity do endereço do grupo vira obsoleta e é apagada no próximo `Identity/get` (`get.rs:208-217`, `:260-268`). Isso é **[código]**, não provado.
- `Identity/set` aceita qualquer endereço da conta, inclusive os do grupo (`crates/jmap/src/identity/set.rs:72-88`).

---

## 7. Envio como o grupo

**[código]**
- **JMAP:** `EmailSubmission/set` lê a Identity na conta da submissão e exige `mailFrom` igual ao e-mail da Identity (`crates/jmap/src/submission/set.rs:508-545`). Depois abre uma sessão SMTP local com o `account_info` dessa conta (`:681-692`). A checagem de remetente aceita qualquer endereço de `authenticated_emails()`, que inclui os dos grupos (`crates/smtp/src/inbound/mail.rs:232-269`; `crates/smtp/src/inbound/auth.rs:214-216`).
  - Pela conta pessoal, com a Identity do grupo: funciona **[provado]**.
  - Pela conta do grupo: funciona **[provado]**.
- **`emailSend`** só é conferido no `AUTH` do SMTP, no token de quem autentica (`smtp/src/inbound/auth.rs:110-120`). No JMAP valem as permissões do membro (`jmapEmailSubmissionCreate` etc.). O grupo **não** precisa de `emailSend` **[provado: grupo só com `emailReceive` enviou]**.
- **SMTP submission (587/465)** autenticado como o membro, com `MAIL FROM: grupo@`, passa pela mesma checagem de `authenticated_emails()` quando `must_match_sender` está ligado. É **[código]**, não provado.

---

## 8. Quota e contadores

**[código]**
- Cada gravação soma o tamanho em `ValueClass::Quota` da conta e, se a conta tem tenant, também em `TenantQuota(tenant)` (`crates/common/src/storage/index.rs:508-516`, `:730-736`). O que chega ao grupo conta no grupo e no tenant dele **[provado: 4860 bytes nos dois]**.
- `quotas.maxDiskQuota` do grupo é aplicado na entrega (`principals.rs:530-540`; `crates/common/src/storage/quota.rs:52-92`). Quota de conta estourada dá `TemporaryFailure "Mailbox over quota."`, e a do tenant, `"Organization over quota."` (`delivery.rs:233-242`).
- `usedDiskQuota` sai no `x:Account/get` (`get.rs:249-257`) e no `x:Tenant/get` (`get.rs:289-295`).
- Os membros **não** gastam quota quando leem ou mexem na caixa do grupo. Os dados ficam na conta do grupo, e o `IndexValue::Quota` é gravado com o tenant da conta dona do dado (`index.rs:508-516`).

---

## 9. Limites de criação

**[código]**
- **Quota de objetos do tenant** (`principal.rs:367-430`): só é aplicada quando o **token de quem cria tem tenant** (`:373`), ou seja, quando quem cria é admin de tenant. Um admin global, como a Gerência, que grava `memberTenantId` explicitamente, **não** passa por esse limite.
  - Criar Group confere `maxGroups` (`:235-243`, `:385-387`).
  - Criar User confere `maxAccounts` (`:210-216`, `:382-384`). `maxAccounts` conta só `@type = User` (`:383`), então **grupos não consomem `maxAccounts`**.
  - Um valor `u32::MAX` significa sem limite (`:379`). Estourar o limite dá `overQuota` (`:419-424`).
- **Licença Enterprise:** em qualquer criação de Account (User ou Group), quando a edição Enterprise está ativa (`principal.rs:46`), `can_create_account()` (`principal.rs:44-55`; `crates/common/src/enterprise/mod.rs:137-153`) compara `total_accounts()` com o limite da licença. `total_accounts()` conta **todos** os `Account` (`crates/common/src/storage/mod.rs:90-92`), então **cada grupo ocupa uma conta da licença**.
- **MailingList** confere `maxMailingLists` pelo mesmo mecanismo (`set.rs:472`).

---

## 10. Apagar e renomear

**Destroy [código]** (`crates/jmap/src/registry/set.rs:671-731`; `write.rs:347-386`):
- Antes de apagar, o servidor busca referências ao objeto (`linked_objects`). Para `Account`, ignora as referências de `PublicKey` e `MaskedEmail` (`set.rs:692-696`). Se sobrar alguma, como um User com o grupo em `memberGroupIds`, a resposta é `objectIsLinked` com a lista dos objetos ligados (`write.rs:374-385`; `set.rs:939-942`) **[provado]**.
- **Como resolver:** um único `x:Account/set` com `update` (tirar `memberGroupIds/<id>` de cada membro) e `destroy` do grupo. As modificações (create/update) são processadas antes do bloco de destroy (`set.rs:337`, `:671`) **[provado]**.
- Quando o destroy dá certo, o servidor revoga as ACLs dadas pela conta (`set.rs:702-709`) e agenda `Task::DestroyAccount` (`principal.rs:441-480`).
- **Retenção:** a tarefa roda imediatamente, a menos que `DataRetention.archiveDeletedAccountsFor` (`structs.rs:884-885`, Enterprise, lido em `crates/common/src/enterprise/config.rs:200-202`) esteja definido. Nesse caso ela é adiada por esse tempo (`principal.rs:449-457`).
- **A tarefa `DestroyAccount`** (`crates/services/src/task_manager/destroy_account.rs:53-157`) apaga PublicKey/MaskedEmail, itens arquivados, o índice de busca e os links de blob (e-mails, arquivos, sieve). Depois chama `danger_destroy_account`, que apaga logs, índices e **contadores** da conta (`crates/store/src/dispatch/store.rs:344-358`) e as ACLs.

**Quota do tenant depois do destroy [código]:**
- Não foi encontrado no código nenhum passo que **subtraia** do `TenantQuota` o uso da conta apagada. O contador do tenant só é refeito pela tarefa `TenantMaintenance/RecalculateQuota` (`crates/services/src/task_manager/maintenance.rs:436-443`, `:514-538`), agendada à mão ou por `StoreMaintenance/ResetTenantQuotas` (`:368-395`). Esse recálculo soma só as contas que ainda existem com aquele tenant.
- **Suspeita (NÃO provada):** `danger_destroy_account` apaga, no `SUBSPACE_COUNTER`, todo o intervalo de chaves `[account_id, account_id+1)` (`store.rs:345-357`). As chaves `TenantQuota(t)` ficam no **mesmo** subespaço, e o prefixo delas é o id numérico do tenant: `Quota` é serializada como `account_id ‖ 0xFF` e `TenantQuota` como `tenant_id ‖ 0xFE` (`crates/store/src/write/key.rs:355-356`, `:591-595`). Se a leitura estiver certa, destruir a conta de id N **zera o `usedDiskQuota` do tenant de id N**, quando esse tenant existir. Antes de confiar no `usedDiskQuota` do tenant depois de apagar contas, convém provar isso no dev01 ou rodar o recálculo.

**Renomear [provado]:** `update` de `name` no Group é aceito. Pelo **[código]**, o índice único de e-mail é trocado (`write.rs:123-132`), e as Identities `grupo-antigo@` dos membros viram obsoletas e são recriadas com o endereço novo no próximo `Identity/get` (`get.rs:180-268`).

---

## 11. MailingList

Definição: `structs.rs:3364-3378`. Validação, índices e patch: `structs_impl.rs:24419-24553`.

- **Campos:** `name`, `domainId` (chave estrangeira para Domain), `description`, `aliases`, `memberTenantId` (chave estrangeira para Tenant, só admin sem tenant pode gravar) e `recipients: Map<String>`. `emailAddress` é `serverSet` (`structs_impl.rs:24525-24553`; `get.rs:276-288`). Qualquer outro campo dá "Invalid property".
- **`recipients`** é um conjunto de **endereços de e-mail**, com validador `Email` (`structs_impl.rs:24543-24545`). Não há referência a Account: membro interno, externo, outra lista ou um grupo entram todos pelo endereço. Para pôr: `"recipients/<email>": true`. Para tirar: `null` (`map.rs:135-200`) **[provado]**. Em JSON Pointer, `~` e `/` no e-mail viram `~0` e `~1`.
- **Expansão [código]:** no `RCPT`, o endereço da lista resolve para `RcptResolution::Expand(recipients)` (`mta.rs:167-171`), e o SMTP troca a lista pelos destinatários (`crates/smtp/src/inbound/rcpt.rs:218-220`).
- **Lista dentro de lista:** é expandida, com controle de ciclo (`visited`) e, quando há aninhamento, sem destinatários duplicados (`seen`, por endereço canônico); sem aninhamento, os `recipients` saem como estão (`mta.rs:204-249`, `:209-218`).
- **Grupo como destinatário de lista:** o endereço resolve para a conta do grupo, que então precisa de `emailReceive` (seção 5).
- **A lista não tem token e não tem caixa.** Não guarda e-mail, não conta quota e não aparece na sessão de ninguém.
- **Tenant:** as mesmas regras da seção 4. Lista com tenant A exige Domain do tenant A. Lista sem tenant num Domain de tenant é aceita, porque não há conferência.
- **Unicidade:** a lista usa o mesmo índice global de e-mail das contas (`structs_impl.rs:24458`).

---

## 12. Limites da API JMAP de gerência

- **No máximo 16 method calls por request**, por padrão (`Jmap.maxMethodCalls = 16`, `structs_impl.rs:23301`; lido em `crates/common/src/config/mailstore/jmap.rs:87`). Acima disso, o parser recusa o request **inteiro** com `urn:ietf:params:jmap:error:limit` / `maxCallsInRequest` (`crates/jmap-proto/src/request/parser.rs:22-40`; `crates/jmap-proto/src/error/request.rs:15-16`; chamada em `crates/http/src/request.rs:139-143`) **[código]**. Lotes maiores precisam ser quebrados em vários requests.
- **`update` (e `create`) de um `/set` é um mapa id → patch e tem de ser objeto JSON.** Os ids do registry podem ser `"0"`…`"9"`, e em PHP `['0' => …]` vira lista JSON `[…]`. O `VecMap` só aceita objeto (`crates/utils/src/map/vec_map.rs:348-375`, só há `visit_map`), e o erro vira `invalidArguments` só daquela chamada (`crates/jmap-proto/src/request/parser.rs:367-368`, `:701-707`) **[código]**. A Gerência força `(object)` (`localmail-gerencia/app/Services/Stalwart/JmapClient.php`, `withObjectUpdateMap`).
- `set_max_objects` padrão: 500 objetos por `/set` (`structs_impl.rs:23304`).

---

## 13. Provado em servidor

Stalwart v0.16.26 do dev01, 2026-10-23.

**Tenant como chave estrangeira**
- User com tenant A em Domain sem tenant → `invalidForeignKey` (Domain).
- Group com tenant A em Domain sem tenant → `invalidForeignKey` (Domain).
- Group sem tenant em Domain do tenant A → aceito.
- User do tenant A + Group sem tenant ou de outro tenant em `memberGroupIds` → `invalidForeignKey` (Account).
- User sem tenant + Group do tenant A → aceito.
- User sem tenant recebe `memberTenantId` A por update → aceito. O update não reconfere o Domain.

**Roles e entrega**
- Group com tenant e roles padrão no dev01 (`defaultGroupRoleIds` = role "Group") → recebeu e-mail.
- Group com `roles: Custom {}`, simulando produção → SMTP aceita `250`. A entrega falha com `Unauthorized access ... emailReceive`, e o remetente recebe bounce `550 This account is not authorized to receive`.
- Group com `roles: Custom {<role sem tenant>}` → `invalidForeignKey` (Role).
- Group **com tenant** (tenant com `roles: Custom {User}`, que tem `emailReceive`) e `roles: Custom {}` + `permissions: {"@type":"Merge","enabledPermissions":{"emailReceive":true}}` → entregue.

**Sessão e envio** (membro autenticado por App Password)
- Conta pessoal: `isPersonal: true`, é a `primaryAccounts` de mail. Conta do grupo: `isPersonal: false`, `isReadOnly: false`.
- A conta pessoal ganha automaticamente a Identity `grupo@`.
- Envio como o grupo funcionou pela conta pessoal (Identity do grupo) e pela conta do grupo. O grupo tinha só `emailReceive`, sem `emailSend`.

**Quota, apagar, renomear, listas**
- A entrega no grupo somou 4860 bytes no `usedDiskQuota` do grupo e no do tenant.
- Destroy de Group com membros → `objectIsLinked`, listando os membros.
- `x:Account/set` com `update` (tirar membros) + `destroy` no mesmo request → membros saem e o grupo é destruído.
- Renomear Group → aceito.
- Patch `recipients/<email>` em MailingList → funciona.

Ficou **sem prova**: o limite de 16 method calls e o `update` como lista JSON (seção 12, lidos no código), envio por SMTP submission como o grupo, acesso IMAP via `Shared Folders`, remoção da Identity quando o membro sai, reconferência de `memberGroupIds` na troca de tenant e o contador do tenant depois do destroy (seção 10).

---

## 14. Docs oficiais × código 0.16.26

Comparado com <https://stalw.art/docs/auth/principals/group/> em 2026-10-23.

| Docs oficiais (grupo) | Código 0.16.26 |
|---|---|
| `type` = `"group"` | `@type: "Group"` dentro de `x:Account` (`structs.rs:12-15`) |
| `members`: indivíduos **e outros grupos** | Não existe. O vínculo fica em `User.memberGroupIds` (`structs.rs:6237`). O Group rejeita o campo (`structs_impl.rs:21723`). |
| `memberOf` e "grupos podem ser aninhados" | Não existe aninhamento. O grupo nasce com `id_member_of` vazio (`principals.rs:563`) e `member_of` vazio no token (`access_token.rs:251`). |
| `emails` (vários endereços) | `name` + `domainId` (endereço principal) + `aliases[]` (`EmailAlias`) |
| `quota` (bytes) | `quotas` (mapa; `maxDiskQuota` e quotas de objetos) |
| `tenant` | `memberTenantId`, chave estrangeira com conferência de tenant. Só admin sem tenant grava. |
| `roles` (lista) | `roles: {"@type":"Default"}` ou `{"@type":"Custom","roleIds":{…}}`. `Default` depende de `defaultGroupRoleIds`. |
| `enabledPermissions` / `disabledPermissions` no topo, "para o grupo **e seus membros**" | Ficam dentro de `permissions: {"@type":"Inherit"/"Merge"/"Replace", …}` e valem **só para o token do grupo**. O membro não herda (`access_token.rs:45-69`). |
| `lists` (listas das quais o grupo faz parte) | Não existe. MailingList guarda endereços em `recipients`, sem vínculo no grupo. |
| `picture` | Não encontrado no código |
| Membros "herdam privilégios, como acessar a caixa compartilhada" | O acesso vem de `member_of` (`is_member`/`has_access`), e não de herança de permissões. |
| Caixa do grupo "aparece como pasta compartilhada no IMAP ou JMAP" | IMAP: `Shared Folders/<grupo@dom>`. JMAP: uma **conta** separada na sessão (`isPersonal: false`), e não uma pasta. |
| Grupo não faz login | Confere: sem `credentials`, o login por diretório recusa conta que não é User e o token do grupo não tem `authenticate`. |
| — (não fala) | `emailReceive` no token do grupo é obrigatório para receber. Enviar como o grupo usa as permissões do membro. Destroy com membros dá `objectIsLinked`. `maxGroups` e a licença contam grupos. |

A página de listas (<https://stalw.art/docs/email/management/mailing-lists>) **está alinhada** com o código: `name`, `domainId`, `emailAddress` read-only, `aliases`, `memberTenantId`, `recipients` como mapa `{email: true}`, listas aninhadas e a exigência de que, "se a lista tem tenant, o domínio é do mesmo tenant". Só não explica a expansão (controle de ciclo e sem duplicados) nem que lista sem tenant em domínio de tenant é aceita.

---

## 15. Receita: como a Gerência cria um grupo

Fonte: `localmail-gerencia/app/Services/Stalwart/GroupService.php` (`createGroup`, `setGroupMembers`, `destroyGroup`) e `JmapClient.php`. A Gerência fala com o Stalwart como admin **sem tenant** e grava `memberTenantId` explicitamente. Por isso não passa por `maxGroups` (seção 9).

**1) Criar o grupo, sem membros:**

```json
{
  "using": ["urn:stalwart:jmap"],
  "methodCalls": [
    ["x:Account/set", {
      "create": {
        "new": {
          "@type": "Group",
          "name": "financeiro",
          "domainId": "<id do Domain>",
          "description": "Financeiro",
          "roles": { "@type": "Custom", "roleIds": {} },
          "permissions": {
            "@type": "Merge",
            "enabledPermissions": { "emailReceive": true },
            "disabledPermissions": {}
          },
          "memberTenantId": "<id do Tenant do cliente>"
        }
      }
    }, "0"]
  ]
}
```

- O Domain precisa ser do **mesmo tenant** (seção 4). Sem tenant, omita `memberTenantId`.
- `roleIds: {}` e `disabledPermissions: {}` têm de sair como **objeto** vazio, e não como `[]`.
- O id do grupo vem em `created.new.id`.

**2) Pôr membros** (num request separado, porque o id do grupo já precisa existir):

```json
["x:Account/set", {
  "update": {
    "<idUser1>": { "memberGroupIds/<idGrupo>": true },
    "<idUser2>": { "memberGroupIds/<idGrupo>": true }
  }
}, "0"]
```

Cada membro com tenant precisa ser do mesmo tenant do grupo. Para tirar um membro, use `null` no lugar de `true`.

**3) Apagar:** num único `x:Account/set`, `update` com `"memberGroupIds/<idGrupo>": null` para cada membro + `"destroy": ["<idGrupo>"]`.

**4) Lista:** `x:MailingList/set` com `name`, `domainId`, `description`, `recipients: {"a@x": true, …}` (ou `{}`) e `memberTenantId` opcional. Para pôr ou tirar um destinatário: `"recipients/<email com ~0/~1>": true|null`.

Ver também `localmail-gerencia/docs/superpowers/specs/2026-10-23-caixas-compartilhadas-design.md` (seção 2.1).
